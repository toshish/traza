# Foresight demo

A run's fate, read from every run before it. The behaviour corpus is loaded,
the [Fate Map](../../docs/guide/foresight.md) finds the path that kills a
support agent's runs, a new run starts down it and is called likely to fail
while it has not failed, and a predictive fuse stops it at the pre-call check
before the next step — while a run on a healthy path goes through.

```sh
examples/foresight/run.sh
```

It builds `traza-server` and `seed` if they are missing (or runs the binary
`TRAZA_BIN` names, and the `seed` beside it), writes the corpus into a
throwaway data directory, starts the server on port 8132 with one `rw` token,
and deletes everything on exit. `python3` is the agent and reads replies;
`curl` carries the operator's requests.

## The corpus

`seed --behaviours` writes runs whose ground truth is known exactly. The
`support-agent` takes one of four paths:

| Path | Steps | Runs | Succeeded |
|---|---|---|---|
| clean refund | llm → `search_orders` → llm → `issue_refund` → llm | 60 | 54 |
| search loop | llm → `search_orders` ×4 → llm | 50 | 5 (5 abandoned) |
| escalation | llm → `lookup_customer` → llm → `escalate_ticket` → llm | 40 | 24 (4 abandoned) |
| double refund | llm → `search_orders` → llm → `issue_refund` ×2 → llm | 30 | 15 |

plus a `researcher` agent, a `ci-fix` coding agent (Claude Code fixing a
failing CI job), and nine runs still in flight, six of them the support
agent's. The demo asserts that the server's `support-agent` counts equal
this table.

## The agent

`agent.py` is a scripted support agent inside a real harness. The model and
tools follow a script — `doomed` re-runs an empty order search, `healthy`
finds the order and refunds it once — so the demo needs no provider. The
harness does what a real one would:

1. before each step, ask `POST /v1/fuses/check` whether the run may take it,
   naming the tool when the step is a tool call, and halt on `stop`;
2. after each step, export its span (`POST /v1/spans`);
3. for the demo's sake, ask `GET /v1/fate/runs/{run}` how runs on the same
   path ended.

Its state lives in a file, so the demo can pause it, add a policy from
outside, and start it again where it left off.

## The beats

| Beat | What to watch for |
|---|---|
| 1 · the history | `GET /v1/fate/cohorts`: 180 completed `support-agent` runs, 98 successes, 9 abandoned, 6 still running — the corpus's ground truth (asserted). |
| 2 · the Fate Map | `GET /v1/fate/map?agent=support-agent` drawn as a tree: every path with its runs, success rate and 95% Wilson interval as a bar against the base rate. Two fatal forks are flagged — `search_orders×3-5` (5 of 50) and `issue_refund×2` (15 of 30) — and the first step is a hotspot. The fork's Wilson interval and its Fisher exact p-value are recomputed with the standard library from the counts (asserted), and every interval printed is checked against the server's. |
| 3 · the doomed path | `ticket-1042` takes three steps with no predictive policy in place. After each, `GET /v1/fate/runs/ticket-1042`: 98 of 180 (on track), 74 of 140 after one search, then — after the second empty search — 5 of 50, `likely_failure`. The run has no outcome yet; its last step is open, so it is pooled with every run that repeated the search at least as often (asserted). |
| 4 · the fuse | A policy sets `min_success_odds: 0.5`. The agent resumes; its pre-call check for a third search answers `stop` with `foresight: runs that took this path and then called tool:search_orders succeeded 5 of 50 times (≤21.4%)`. Asserted: the reason, `raised: true`, the upper bound below the limit, the map being the one beat 2 cached, `tripped_by: null` (a prediction, not a latch), and the server holding the run's three steps — in the usage the check judged and on its tape after the agent exited — so no fourth search ran. |
| 5 · the healthy path | `ticket-1043` refunds once and finishes; every check answers `continue`; the checks that name a tool are judged with it appended (69 of 90 before the refund), and a model call's, which names none, on the path so far (asserted). Read back after it ends: `success`, judged at its own node (54 of 60). |
| 6 · calibration | The map's self-assessment: Brier score, baseline and skill per depth (skill recomputed and asserted — zero at depth 1, where every run looks the same, and above 20% by depth 3), and the reliability bins at depth 3: what the map said against what happened, each observed rate drawn with its Wilson interval, recomputed from the bin's counts, and whether the prediction falls inside it. |

## Knobs

| Variable | Default | Meaning |
|---|---|---|
| `TRAZA_DEMO_PORT` | `8132` | Server port |
| `TRAZA_BIN` | `./target/release/traza-server` | The server binary: a path, taken from your current directory, or a command name on `PATH`. The default is built with cargo when missing |
| `TRAZA_SEED_BIN` | `seed` beside the server binary | The binary that writes the corpus (`cargo install` puts it beside `traza-server`) |

## Honest caveats

- **The corpus is built to have a fatal fork.** Its outcomes are assigned by
  branch, so the map's skill here is high by construction. On real history,
  read `calibration` first: an agent whose outcome does not depend on its
  path shows skill near zero, and its foresight is then no better than its
  base rate.
- **Correlation, not cause.** The fork says runs that re-ran the search
  failed; it does not say the re-run caused it. The fork's example runs — a
  success and a failure from the same point — are printed for
  [`GET /v1/diff`](../../docs/guide/replay.md).
- **The fuse acts only when the harness asks.** A predictive stop is answered
  on the pre-call check and on verdict queries, never on an ingest
  acknowledgement. An agent that does not call the check is not stopped.
- **The fuse reads cached maps only.** Beat 2's request builds and caches the
  map; the demo waits (at most 15 s, normally not at all) for the fuse to see
  one before it resumes the agent.
- The token counts and costs on the agent's spans are fixed by the script.
  Every verdict, count, rate and interval printed is the server's answer,
  asserted or recomputed — none is a constant. The one default quoted,
  `foresight_min_support`'s 30, is printed as the documented default it is.

## Runtime

About 1.5 s on an Apple M1 Max laptop. There is no scale knob: CI runs it as is.
