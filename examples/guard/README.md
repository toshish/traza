# Guard demo

A runaway agent, stopped between steps. A research agent loops on a failing
tool, and a [run fuse](../../docs/guide/guard.md) stops it — then the kill
switch, a reset, a grant, and the same fuse in front of Claude Code.

```sh
examples/guard/run.sh
```

It builds `traza-server` if it is missing (or runs the binary `TRAZA_BIN`
names), starts it on port 8129 with one `rw` token and a `--pricing` file, and
deletes the throwaway store on exit. `python3` is the agent and reads replies;
`curl` carries the operator's requests.

This directory holds two things: the demo (`run.sh`, `agent.py`) and the
[Claude Code hook](#the-claude-code-hook) (`claude-code-hook.sh`), which the
demo runs in its last beat and which you can install in your own setup.

## The agent

`agent.py` is a scripted model inside a real harness. The model asks for
`fetch_page` until it has read the page, then answers; its token counts are
fixed by the script, growing by 400 prompt tokens with every tool result in
the context — which is what makes a retry loop expensive, since each failed
attempt is resent with every attempt before it. The harness does what a real
one should with fuses:

1. before each step, ask `POST /v1/fuses/check` whether the run may take it,
   with the step's estimated cost priced from the same pricing file the server
   uses, and halt on `stop`;
2. after each step, export its spans and read the acknowledgement — the
   `Traza-Fuse` header and the `fuses` field — and halt on `stop`.

Its state lives in a file, so the demo can stop it, act on the run from
outside, and start it again where it left off.

## The beats

| Beat | What to watch for |
|---|---|
| 1 · the policy | `research-agent` runs stop at `max_repeats: 4` or `max_cost_usd: 0.035`, priced from `--pricing` (sim-large at $3.00 / $15.00 per million tokens). |
| 2 · the loop | Four identical `fetch_page` calls, each answered 503, each step dearer than the last. The fourth step's acknowledgement is printed verbatim: `Traza-Fuse: stop; run=sess-loop` and a `fuses` field naming `max_repeats 4 reached`. Asserted: the agent stopped at step 4, the spans of that step were stored. |
| 3 · the verdict | `GET /v1/fuses/runs/sess-loop`: reason, usage, `top_repeat`, and the evidence span read back by id — the fourth `fetch_page`, status `error`. The server's priced cost must equal the agent's own sum from the same pricing file. |
| 4 · the gate | The agent is restarted on the same run. Its pre-call check answers `stop` (the stop is sticky), and the run's recorded step count is unchanged: nothing ran. |
| 5 · the kill switch | A second, healthy run — distinct calls, all succeeding — takes two steps; `POST /v1/fuses/runs/sess-healthy/trip` stops it; its next pre-call check is refused with `manually tripped: …`. |
| 6 · reset, then a grant | The upstream is fixed and the looping run reset. The reset answers `warn` — the loop spent 87% of the budget, and a reset does not refill it — so the next step's estimate is refused: `max_cost_usd 0.035 would be exceeded by the next step: $0.0306 spent + $0.01065 estimated`. A reset with `{"grant": {"max_cost_usd": 0.10}}` approves it; the agent finishes, past the policy's $0.035 and within the grant (asserted). |
| 7 · Claude Code | `claude-code-hook.sh` fed the JSON Claude Code pipes to a hook: a fresh session's PreToolUse exits 0; three PostToolUse records of the same `npm test` stop the session (the third exits 2); the next PreToolUse exits 2 with the reason on stderr — what the model is shown instead of a fourth test run. |
| 8 · the record | `GET /v1/fuses/events`: every transition above, in order, as an alert dispatcher polling the feed would see it. |

## Knobs

| Variable | Default | Meaning |
|---|---|---|
| `TRAZA_DEMO_PORT` | `8129` | Server port |
| `TRAZA_BIN` | `./target/release/traza-server` | The server binary; built with cargo when the default is missing |

## Honest caveats

- **The model is scripted; the harness is not.** Because the script fixes
  every token count, the agent's cost estimate equals what the step then costs,
  so beat 6's arithmetic is exact. A real agent's estimate is a guess, and the
  check is only as good as the guess.
- **This agent exports synchronously, one step per batch.** So the
  acknowledgement of step 4 arrives before step 5 could start. An exporter that
  batches — the OpenTelemetry SDK sends every 5 seconds by default — hears a
  stop one or more steps late. The pre-call check is what refuses a step
  before it runs, and only if the agent asks.
- **A step is never interrupted.** Every verdict above acts between steps.
- **The hook's spans carry no cost or tokens**, so its session is held to the
  repeat limit only; beat 7 does not exercise cost limits through Claude Code.
- The policy limits, prices and token counts are chosen so the beats land in
  a few steps; the verdicts, reasons, costs and counts printed are the
  server's own answers, asserted, not constants.

## Runtime

About 3 s on an Apple M1 Max laptop. There is no scale knob: CI runs it as is.

## The Claude Code hook

A run fuse before every tool call: [`claude-code-hook.sh`](claude-code-hook.sh)
puts Traza's [run fuses](../../docs/guide/guard.md) in front of Claude Code's
tool calls.
The run is the Claude Code session: its id is the run id Traza checks.

- As a **PreToolUse** hook it is the pre-call gate. Before each tool call it
  asks `POST /v1/fuses/check` whether the session may take another step,
  naming the tool. On a `stop` verdict it exits 2 with the reasons on stderr:
  Claude Code blocks the call and shows the reasons to the model. A `warn`
  is printed and the call proceeds.
- As a **PostToolUse** hook (optional) it records each tool call that ran as
  a span — `session.id` set to the session, `gen_ai.operation.name:
  execute_tool`, the tool's name, and its input — so the session's step and
  repeat limits have something to count. When the acknowledgement says the
  run is now stopped it exits 2 at once, so the model hears it after the
  call that crossed the limit; that call has already run, and the next one
  is the one the gate refuses.

Without the PostToolUse half, a verdict covers whatever Traza already holds
under the session id — spans from any instrumentation that sets
`session.id` (or another [recognized session key](../../docs/llm-semantics.md))
to it — and the kill switch, which works with nothing recorded at all:

```sh
curl -X POST http://127.0.0.1:8080/v1/fuses/runs/$SESSION_ID/trip \
  -d '{"reason": "stop this session"}'
```

### Install

Requires POSIX `sh`, `curl` and `python3`. In `.claude/settings.json` (or
`~/.claude/settings.json` for every project):

```json
{
  "hooks": {
    "PreToolUse": [
      {"matcher": "*", "hooks": [{"type": "command", "command": "/path/to/claude-code-hook.sh"}]}
    ],
    "PostToolUse": [
      {"matcher": "*", "hooks": [{"type": "command", "command": "/path/to/claude-code-hook.sh"}]}
    ]
  }
}
```

Then give the session a policy. Out of the box the server's global policy
only **warns** on 12 repeats of the same call with the same input; to stop a
looping session instead, either start the server with
`--fuse-max-repeats 5 --fuse-action stop`, or create a policy for the
service the PostToolUse spans carry (`claude-code`):

```sh
curl -X POST http://127.0.0.1:8080/v1/fuses/policies -d '{
  "name": "claude-code", "match": {"service": "claude-code"},
  "limits": {"max_repeats": 5, "max_steps": 400}, "action": "stop"}'
```

A session stopped by a limit stays stopped until someone resets it:

```sh
curl -X POST http://127.0.0.1:8080/v1/fuses/runs/$SESSION_ID/reset \
  -d '{"grant": {"max_steps": 800}}'
```

### Configuration

| Variable | Default | Meaning |
|---|---|---|
| `TRAZA_URL` | `http://127.0.0.1:8080` | The server. |
| `TRAZA_TOKEN` | none | Bearer token, when the server enforces auth. A `ro` token is enough for the gate; recording needs `rw`. It is passed to curl through a here-document, never on a command line. |
| `TRAZA_TENANT` | the default tenant | The run's tenant, for an unbound token. A bound token is forced to its own. |
| `TRAZA_RECORD_TOOL_INPUT` | off | `1` records each tool's full input on its span. By default only a SHA-256 of the canonical input is recorded — enough for repeat detection, without copying commands and file contents into the store. |
| `TRAZA_FUSE_FAIL_CLOSED` | off | `1` blocks the tool call when Traza cannot answer — unreachable, or an error instead of a verdict, which is what the gate gives for a run it could read only part of and that part does not stop ([why](../../docs/guide/guard.md#what-is-durable-and-what-is-rebuilt)). By default the hook fails open: it prints a warning and lets the call through. |
| `TRAZA_TIMEOUT` | `5` | Seconds to wait for Traza. |
| `CLAUDE_SESSION_ID` | the hook input's `session_id` | The run id. |

### What it does not do

- **The gate cannot see inside a step.** It answers before the call from
  what has been recorded; a single tool call that runs away on its own is
  not interrupted.
- **Recorded spans carry no duration.** A PostToolUse hook fires after the
  call, without its start time, so each span starts and ends at the moment
  it is recorded; `max_duration_seconds` then measures from the first
  recorded call to the last.
- **Cost and token limits need cost and token data.** The PostToolUse spans
  carry neither; those limits act only on spans from instrumentation that
  records model usage under the same session id.
- **Fail-open is the default.** A guard that blocks all work when the guard
  is down is a choice, and `TRAZA_FUSE_FAIL_CLOSED=1` is where to make it.

Beat 7 of the demo above runs this script against a real server, and
`tests/guard.rs` does so in more detail: a fresh session
passes, three identical recorded calls stop it under a repeat limit of
three, the gate then refuses, a reset lets it through, the kill switch
stops it again, and an unreachable server fails open (or closed, when
asked).
