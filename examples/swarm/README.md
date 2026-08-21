# Swarm demo

A live agent cockpit in under a minute. It starts a Traza server on a
throwaway store, then streams a simulated agent platform into it **in real
time** — every span carries wall-clock nanosecond timestamps and is posted in
the batch tick after it finishes — so the dashboard you open is breathing:
live tail scrolling, fan-out waterfalls with a visible critical path, sessions
with readable conversations, token and cost analytics accruing as you watch.
The pace is deliberately busy: a few dozen spans a second, with bursts when a
fan-out lands and a periodic retry storm for the failures screen.

```sh
examples/swarm/run.sh
```

It builds `traza-server` if it is missing, starts it on port 8124 with WAL
durability, prints the dashboard links, and streams until Ctrl-C. Nothing
persists and no configuration is touched. `python3` (stdlib only) generates
the stream.

## The dashboard

The deep links point at the dashboard, which is a built artifact the server
serves from `ui/dist`. On a fresh clone it does not exist yet, so the links
answer a JSON 404 until you build it once:

```sh
cd ui && npm ci && npm run build
```

The server picks it up without a restart. The demo probes `GET /` after
startup: when the dashboard is missing it says so under the banner, and
`TRAZA_SWARM_OPEN=1` will not open a browser onto a 404. The API, the
stream and the verification gate work either way.

## What is streaming

Three services with distinct trace shapes, batched to `/v1/spans` every
~300 ms:

| Service | Shape |
|---|---|
| `support-agent` | A dozen concurrent multi-turn chats, a stable `session.id` per conversation. Each turn: an `agent.turn` root, a short tool-choice model call, one to three tool calls (`kb.search`, `memory.recall`, `crm.lookup`), and an answering model call (`gpt-4o-mini` / `claude-sonnet-4-5` / `gemini-2.0-flash`, some streaming) carrying `gen_ai.input.messages` / `gen_ai.output.messages`, so the conversation view renders a readable chat |
| `research-swarm` | Every 6-9 s, one fan-out: a planner model call, then 5-10 **genuinely concurrent** workers doing `web.fetch` (with the occasional 503 and retry) plus a per-worker summarize call, then a reduce call. The waterfall showpiece and the burst in the span rate — its `#/trace/…` link prints when it lands |
| `code-agent` | Tool-heavy, longer traces every second or two, so several are in flight at once: `fs.read`, a model call, `patch.apply`, `tests.run` (which sometimes fails), `git.diff` |

On top of the steady stream, every half-minute or so one of the three falls
into a short **retry storm**: a single tool erroring five to eight times in
one trace, with visible backoff between attempts, then recovering. The
services take turns (`kb.search`, `web.fetch`, `git.fetch`).

Costs are metered on each span as `llm.cost_usd`, computed by the generator
from per-model rates the way an instrumented client would meter them — so
analytics shows crisp metered costs, not derived approximations, and the
total visibly ticks up while you watch `#/analytics`.

## The beats

1. **Banner** — deep links to `#/tail`, `#/overview`, `#/failures`,
   `#/analytics`. Open them before the stream starts; the 15-minute
   overview window fills in front of you because the timestamps are now,
   not replayed history.
2. **Status lines** every ~3 s. Spans acknowledged (summed from the
   server's `accepted` responses), the span rate derived from them, and
   cost so far are server-read; sessions started is the generator's own
   count of conversations it has opened.
3. **Research fan-outs** print their trace deep link as they complete —
   open one for the waterfall and its critical path. The worker and span
   counts in the notice are what the generator emitted.
4. **Retry storms** print a notice as they resolve — one tool on one
   service erroring five to eight times in a single trace, then
   recovering. Watch it land on `#/failures`, or flip the live tail to
   errors only. The error count in the notice is the generator's own
   tally.
5. **Verification at ~6 s**, every number read back from the server, exit
   non-zero if any fails: `/v1/sessions` lists at least 5 sessions,
   `/v1/stats/llm` has at least 3 models with `cost_usd > 0`, the first
   research trace has at least 20 spans with measured worker concurrency of
   at least 4, and one session's chat turns parse back in the exact shape
   the conversation view renders — `{role, parts: [{type, content}]}`,
   every part checked for both keys.
6. **Summary on exit** (Ctrl-C or the duration knob), split into what it
   is: server-read figures (spans acked, sessions stored from
   `/v1/sessions`, total cost and top model by spend from `/v1/stats/llm`)
   and the generator's own tallies (sessions started, fan-outs, retry
   storms, error spans emitted, spans still in flight).

## Knobs

| Variable | Default | Meaning |
|---|---|---|
| `TRAZA_DEMO_PORT` | `8124` | Server port |
| `TRAZA_SWARM_SECONDS` | `0` | Run duration; `0` streams until Ctrl-C. Non-zero values below 8 are raised to 8 so the verification gate always runs |
| `TRAZA_SWARM_TEMPO` | `1.0` | Positive multiplier on the emission rate. It scales the gaps between turns, traces, fan-outs and storms; span durations stay plausible regardless. `1.0` is the lively default; `0.1` approximates the old quiet pace; `2.0` roughly doubles the churn |
| `TRAZA_SWARM_CHATS` | `12` | Concurrent support conversations (minimum 3, so the session assertion stays meaningful) |
| `TRAZA_SWARM_OPEN` | `0` | `1` opens the dashboard with `open`/`xdg-open` — only when the startup probe saw the built dashboard answer 200; default prints links only |

Smoke / CI mode:

```sh
TRAZA_SWARM_SECONDS=8 examples/swarm/run.sh
```

## Honest caveats

- **The workload is simulated.** Conversations are scripted, token counts are
  estimated from the text, and `llm.cost_usd` is computed by the generator
  from a built-in rate table — these are not real provider bills.
- **Two kinds of numbers print, labeled as what they are.** The server-read
  figures — spans acked, span rate, cost, sessions stored, and everything in
  the verification gate — come back from the live server, never echoed from
  what was sent. The generator's own tallies — sessions started, fan-outs,
  the fan-out notice's worker and span counts, the storm notice's error
  count, error spans emitted, spans still in flight — are its emission
  counters: honest bookkeeping of what it sent, not server measurements.
- A few percent of spans are injected errors (failed fetches, failed test
  runs, the odd tool timeout, plus the periodic retry storm); the exact
  emitted count is printed in the summary.
- Span durations are drawn from plausible ranges, not sampled from any real
  system's latency distribution.
- The stream posts each span after its end time passes, so at exit a handful
  of spans are always still in flight; the summary states how many.

## Runtime

Continuous by default — a few seconds to the first fan-out link, verification
at ~6 s, the first retry storm inside the first half-minute, then it streams
until Ctrl-C (the summary prints on the way out). Smoke mode
(`TRAZA_SWARM_SECONDS=8`) finishes, verification and summary included, in
about 9 seconds.
