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

This directory holds the demo (`run.sh`, `agent.py`). Its last beat runs the
[Claude Code hook pack](#the-claude-code-hook), which you can install in your
own setup.

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
| 7 · Claude Code | [`traza_hook.py`](../claude-code/README.md) fed the JSON Claude Code pipes to a hook: a person's nudge queued for the session rides its first PreToolUse as `additionalContext` (no permission decision); three recorded PostToolUse calls of the same `npm test` stop the session (the third answers `decision: "block"`); the next PreToolUse answers `permissionDecision: "deny"` with the reason — what the model is shown instead of a fourth test run. |
| 8 · the record | `GET /v1/fuses/events`: every transition above, in order, as an alert dispatcher polling the feed would see it. Asserted: exactly six events — the loop's `stop`, the healthy run's `manual_stop`, the plain `reset` and the `warn` it leaves, the `reset` with its grant, and the hook session's `stop`. |

## Knobs

| Variable | Default | Meaning |
|---|---|---|
| `TRAZA_DEMO_PORT` | `8129` | Server port |
| `TRAZA_BIN` | `./target/release/traza-server` | The server binary: a path, taken from your current directory, or a command name on `PATH`. The default is built with cargo when missing |

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
  a few steps. The verdicts, reasons, usage counts and run costs printed are
  the server's own answers, asserted, not constants. The per-step prices are
  the agent's own arithmetic on the pricing file; what is asserted is that the
  server's cost for the run equals their sum.

## Runtime

About 3 s on an Apple M1 Max laptop. There is no scale knob: CI runs it as is.

## The Claude Code hook

The fuse in front of Claude Code's tool calls is a pack of its own,
[`examples/claude-code`](../claude-code/README.md): a PreToolUse gate that
denies a stopped session's next call and relays a person's
[nudges](../../docs/guide/steer.md#nudges), a SessionStart summary of earlier
runs, an optional tool-call recorder, and an installer that merges it into
`.claude/settings.json`. Beat 7 above runs it against the demo's server.

