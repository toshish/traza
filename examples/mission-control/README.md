# Mission Control demo

Live agent runs you can steer. It starts a Traza server on a throwaway store,
gives it the history its [foresight](../../docs/guide/foresight.md) learns
from, then runs simulated support agents in real time. Each one asks the
[run guard](../../docs/guide/guard.md) before every step — exactly as the
SDK guards and the Claude Code hook pack do — hears
[nudges](../../docs/guide/steer.md#nudges) there, and holds on a stop until
someone releases it. Open the dashboard's **Live** screen and the runs are
moving: steps ticking, spend counting up, foresight ranking what is likely to
fail.

```sh
examples/mission-control/run.sh
```

It builds `traza-server` if it is missing (or runs `TRAZA_BIN`), starts it on
port 8134, checks every claim below against the server, prints the links, and
keeps the agents running until Ctrl-C. Nothing persists. `python3` (standard
library only) runs the agents.

The links point at the dashboard, a built artifact the server serves from
`ui/dist`; on a fresh clone build it once with
`cd ui && npm ci && npm run build`. The demo says so when it is missing.

## What is running

- **History.** 180 completed `support-agent` runs down four paths, finished
  hours ago with their outcomes declared: a clean refund (54 of 60 succeeded),
  a search re-run four times (5 of 50), an escalation (24 of 40), a double
  refund (15 of 30). Written, then the server is restarted, so the live view
  opens empty while the Fate Map reads them from the store.
- **Support agents** (`tkt-…`, service `support-agent`) taking the same
  paths, plus the occasional tool no earlier run took. Before each step they
  `POST /v1/fuses/check` naming it; each span is posted as its step ends, with
  the messages, tool arguments and results, and a metered cost. Told by an
  advice nudge to move on, a looping agent stops searching and issues the
  refund; a block nudge skips the step.
- **A support team** (`conv-…`, service `support-app`): `triage`, `billing`
  and `refunds` agents handing a ticket on, and sometimes `triage` and
  `billing` handing it back and forth.
- **Two policies.** `support-agent` runs stop when their path's odds fall
  below 0.3 (`min_success_odds`, a predictive fuse); `support-app` runs stop
  at three handoff cycles (`max_handoff_cycles`). A stopped agent waits 90
  seconds for someone to release it, then abandons the run. One deadline
  covers the refused step however often the check refuses it, and the agent
  counts itself released only when the check lets that step go: its verdict
  read judges the path so far, without the step, so a stop on the step alone
  reads `continue` there and is no release.

## The beats

Each claim is read back from the server; the script exits non-zero if one
fails.

1. **Foresight on the live view** — `GET /v1/runs/live` lists a looping run
   as a likely failure (the share of earlier runs on its path that succeeded,
   with its interval, against the agent's base rate) and another as on track.
2. **The stream** — `GET /v1/runs/live/stream` opens with a snapshot and
   sends a row as a run changes.
3. **A run held by its history** — a looping run's pre-call check answers
   `stop` with a foresight reason, while the live listing shows only its next
   step pending: the listing never carries a predictive stop, which is why the
   dashboard asks the run's verdict.
4. **A nudge, and an approval** — an advice nudge waits as pending while the
   run holds; a reset with a grant of lower odds releases it; the agent hears
   that nudge at its next check and moves on to the refund; the nudge lists as
   delivered.
5. **The kill switch** — a healthy run is tripped by hand, its next check
   answers `stop`, and a reset lets it carry on.
6. **Agents bouncing a ticket** — a ping-pong run is stopped at three handoff
   cycles, and its topology counts the cycles.
7. **A fork** — a finished refund run is forked at its order search with an
   edited result (no order found), live after it; the script plays the fork's
   harness by hand (the replay's model route, `tools/next`, then a stand-in
   provider past the cut), and `GET /v1/forks` lists the fork, compared with
   its parent.

Then the agents keep going. On the Live screen: nudge a held run and watch
it move on, approve it with `min_success_odds` 0.05, pull the kill switch;
open a run for its agent graph, its nudges, and **Fork from here** on any
step.

## Knobs

| Variable | Default | Meaning |
|---|---|---|
| `TRAZA_DEMO_PORT` | `8134` | Server port |
| `TRAZA_LIVE_SECONDS` | `0` | How long the agents run after the checks; `0` runs until Ctrl-C |
| `TRAZA_LIVE_TEMPO` | `1` | A multiplier on the agents' pace; CI runs `2` |
| `TRAZA_BIN` | — | A prebuilt `traza-server` to run instead of building one |

Smoke / CI mode, about 20 seconds:

```sh
TRAZA_LIVE_SECONDS=1 TRAZA_LIVE_TEMPO=2 examples/mission-control/run.sh
```

## Honest caveats

- **The agents are simulated.** Their conversations are scripted, token
  counts are made up, and `llm.cost_usd` comes from a rate in the script, not
  a provider's bill. What they do with a nudge is the script's choice; a real
  agent decides for itself.
- **The history is made to order.** Its paths and outcome counts are chosen
  so foresight has a clear story; a real agent's map is as informative as its
  history is ([calibration](../../docs/guide/foresight.md#how-good-is-this-map-calibration)).
- **The fork's live half is a stand-in.** Traza never calls a model; past the
  cut the script answers for the provider, as your harness would call it.
- **The store is open.** No credential is minted, so the dashboard needs no
  token; on loopback only.
