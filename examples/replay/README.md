# Replay demo

Record an agent run, then [replay](../../docs/guide/replay.md) it through the
agent's own code with zero model calls.

```sh
examples/replay/run.sh
```

It builds `traza-server` if it is missing (or runs the binary `TRAZA_BIN`
names), starts it on port 8130 with an `rw` token for the agent's telemetry and
an `ro` token for everything else, and deletes the throwaway store on exit.
`python3` is the agent, the stand-in model and the JSON reader; `curl` carries
the rest.

## The pieces

- **`agent.py`** — a weather assistant written with `urllib` and nothing else,
  on the OpenAI chat-completions wire and configured the way the OpenAI SDK is:
  `OPENAI_BASE_URL` and `OPENAI_API_KEY`. It calls the model, runs the
  `get_weather` tool the model asks for, sends the result back, and records
  the run as OpenTelemetry GenAI spans: `gen_ai.input.messages` and
  `gen_ai.output.messages` in the semantic conventions' `parts` shape, token
  usage, the tool call's arguments and result.
- **`model.py`** — a scripted stand-in provider on an ephemeral port, so the
  recording needs no key and no network. Deterministic: the same prompt gets
  the same answers, and a prompt asking for Fahrenheit gets a different one.

The agent never knows which of the two — the stand-in or a Traza replay — it
is talking to. That is the point.

## The beats

| Beat | What to watch for |
|---|---|
| 1 · record | The agent runs against the stand-in model as session `weather-1`: a tool call, the tool, the answer. `GET /v1/tapes/weather-1` shows three steps — `llm`, `tool`, `llm` — with their digests; the workflow wrapper span is context, not a step. The stand-in counts two calls. |
| 2 · replay | The stand-in is stopped, and the script checks its port now refuses connections. `POST /v1/replays` (with the `ro` token) snapshots the run; the same agent code runs with `OPENAI_BASE_URL` set to the replay's `base_urls.openai` and the `ro` token as its key. Asserted: both model answers and the final answer are identical to the recording; every model call the agent made went to the replay URL; `GET /v1/replays/{id}` reports `served 2`, `remaining 0`, both calls `matched`, `diverged null`; the stand-in's call count is unchanged. |
| 3 · strict refusal | A fresh replay, and the agent with its v2 system prompt. The first call is refused `409` with `x-should-retry: false` and `{"error":{"type":"replay_divergence","step":0,…}}`, the expected and received digests beside it. The step is not consumed: the replay then reports `served 0`, `remaining 2`, `diverged` at step 0. |
| 4 · diff | The stand-in is started again and the v2 prompt runs for real as `weather-2`. `GET /v1/diff?a=weather-1&b=weather-1-replay` — the replayed run, recorded as it ran in beat 2 — is `same` at every step, `first_divergence: null`. `GET /v1/diff?a=weather-1&b=weather-2` names the first divergence: step 0, kind `input`; the tool step in between is `same`, since the city did not change. |

## Knobs

| Variable | Default | Meaning |
|---|---|---|
| `TRAZA_DEMO_PORT` | `8130` | Server port. The stand-in model takes an ephemeral port of its own |
| `TRAZA_BIN` | `./target/release/traza-server` | The server binary; built with cargo when the default is missing |

## Honest caveats

- **The model is a stand-in, not a model.** With a real provider, the
  recording in beat 1 costs real calls and the replay in beat 2 costs none;
  the replay serves whatever was recorded and cannot tell the difference.
- **"Zero model calls" is proven from the agent's side.** The stand-in is
  stopped and refuses connections, every model call the agent made went to
  the replay URL, and the stand-in's counter did not move. That the Traza
  server makes no outbound request of its own is a property of the replay
  routes, which have no HTTP client; the demo does not watch the server's
  sockets.
- **The tool runs live, every time.** `get_weather` ran in all three
  completed runs. A replay answers model calls, not tool calls; a harness that
  wants recorded tool results takes them from
  `POST /v1/replays/{id}/tools/next`, which this demo does not exercise.
- **The instrumentation is hand-written** in `agent.py` — the attributes an
  OTel GenAI instrumentation records, exported as one batch when the run ends
  — so the demo needs no OpenTelemetry SDK.
- **The divergence is at step 0 because the system prompt is in the first
  request.** A change that first shows up later — a different tool result,
  say — is refused at the first step whose messages differ. Strict mode
  compares messages only: a changed model name or `tools` list is served.

## Runtime

About 2 s on an Apple M1 Max laptop. There is no scale knob: CI runs it as is.
