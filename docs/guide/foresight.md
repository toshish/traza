# Foresight: the Fate Map and predictive fuses

Traza holds every step of every run an agent made. Read one at a time, those
runs are traces. Read together, they say something no single trace can:
**which paths an agent takes, how often each path ends well, and exactly
where the paths that fail part from the ones that do not.** This guide covers
three things built on that:

- the **Fate Map** — a tree of every path an agent's completed runs took,
  each branch with its success rate, a 95% interval, its cost, and example
  runs; the branches that kill runs (**fatal forks**) and the places the agent
  cannot make up its mind (**hotspots**) are flagged;
- **foresight** for one run — its path so far walked down the map: "runs that
  took this path succeeded 5 of 50 times (10.0%, 95% CI 4.3–21.4%)", a
  verdict, and what earlier runs did next from here;
- **predictive fuses** — a fuse limit, `min_success_odds`, that stops (or
  warns) a run whose path has historically failed, at the pre-call check,
  before it spends more.

Nothing here calls or embeds a model. It is counting, done carefully: every
number below is arithmetic over stored spans, and every route says how much
history it stands on. The wire is in the
[HTTP reference](http-api.md#foresight); the MCP tool is
[`foresee_run`](mcp.md).

## See it

`seed --behaviours` loads a corpus whose ground truth is known exactly: a
`support-agent` whose runs take one of four branches —

| Branch | Steps | Runs | Succeeded |
|---|---|---|---|
| clean refund | llm → `search_orders` → llm → `issue_refund` → llm | 60 | 54 |
| search loop | llm → `search_orders` ×4 → llm | 50 | 5 (5 abandoned) |
| escalation | llm → `lookup_customer` → llm → `escalate_ticket` → llm | 40 | 24 (4 abandoned) |
| double refund | llm → `search_orders` → llm → `issue_refund` ×2 → llm | 30 | 15 |

— a `researcher` agent that picks one of four sources (9, 7, 3 and 1
successes in 10), and six `support-agent` runs still in flight. Every
response below is real output from a server over that corpus, trimmed where
marked.

```sh
seed --data-dir ./data --behaviours
traza-server --data-dir ./data
```

## See it run

[`examples/foresight/run.sh`](../../examples/foresight/README.md) runs this
guide against a real server in a couple of seconds. It loads the corpus
above, draws the `support-agent`'s Fate Map as a tree with every branch's
Wilson interval, and recomputes the fatal fork's interval and Fisher test
from its counts. A scripted agent then starts a new run down the search
loop: after its second empty search, `GET /v1/fate/runs/{run}` calls it
`likely_failure` while it has no outcome yet. A policy with
`min_success_odds: 0.5` stops it at the pre-call check before a third
search, with the foresight reason, and a run on the clean-refund path is
allowed every step. Last, it prints the map's calibration. The counts it
prints are asserted against the corpus's ground truth, and it exits non-zero
if any claim fails.

## Behaviour paths

A run's **steps** — its LLM, tool and agent spans
([step kinds](../llm-semantics.md#steps-and-runs)), in start order — become
**tokens** `kind:name`: `llm:gpt-4o`, `tool:search_orders`,
`agent:researcher`. That is the step's signature without its input hash, so
two calls of one tool are the same behaviour whatever their arguments.
Consecutive repeats of one token fold into a single token by bucket:

| Repeats | Token |
|---|---|
| 1 | `tool:search_orders` |
| 2 | `tool:search_orders×2` |
| 3 to 5 | `tool:search_orders×3-5` |
| 6 or more | `tool:search_orders×6+` |

so a loop is one shape, not a tree that grows a level per iteration. A path
holds at most 64 tokens; anything deeper ends in a terminal `…` token. A run
read only in part — more than 192 steps, or past a build's bounds — ends in
`…` too, after dropping its last token (whose repeat bucket might be larger
than what was read). Tokens are matched by step and bucket, never by their
text, so a tool literally named `x×2` is not the same branch as `x` called
twice. Names longer than 160 bytes — tool, model and agent names alike — are
cut there (ending in `…`) the same way everywhere they are read.

## Cohorts

Runs are compared only with runs of the **same agent in the same tenant**.
The agent is the first step's agent name (`gen_ai.agent.name`, or the
Traceloop/OpenInference equivalents), else the service of the run's earliest
span. So the corpus's researcher runs, rooted in an `invoke_agent` span that
names `researcher`, form the cohort `researcher`, not `research-svc`.

A run may also declare its **task** with `traza.task`, else `session.goal`.
The value is hashed to a 16-hex key; tasks with at least three runs are
listed under their agent, and a map can be narrowed to one (`task=`).

```sh
curl 'http://127.0.0.1:8080/v1/fate/cohorts'
```

```json
{"bounds":{"cohorts_dropped":0,"node_cap":false,"runs_dropped":0,"runs_partial":0,"runs_tracked":226,"signature_cap":false,"spans_read":1342,"spans_skipped":0,"window_narrowed":false},"built_at_ns":1792298013667246000,"cohorts":[{"abandoned":9,"agent":"support-agent","base_rate":0.5444,"failure":73,"runs":180,"success":98,"tenant":"","unknown":6},{"abandoned":4,"agent":"support-agent","base_rate":0.6,"failure":12,"runs":40,"success":24,"task":"dd8fe599d8750716","tenant":"","unknown":0},{"abandoned":0,"agent":"researcher","base_rate":0.5,"failure":20,"runs":40,"success":20,"tenant":"","unknown":0}],"truncated":false,"window":{"since_ns":1789706013667246000,"until_ns":1792298013667246000}}
```

**Outcomes** come from the same rule every other run surface uses
([attribution](../llm-semantics.md#steps-and-runs)): the latest declared
`session.outcome`, else the status of the span that ended last — `success`,
`failure` or `abandoned`. A run with no declaration whose last span ended in
the last 15 minutes is still active, and a declared outcome Traza does not
recognise cannot be told; both are **`unknown`, excluded from every rate, and
counted** (the six live runs above). The base rate is
`success / runs` over completed runs.

## The Fate Map

```sh
curl 'http://127.0.0.1:8080/v1/fate/map?agent=support-agent'
```

The answer is the cohort's counts, what bounded the build, the window, the
tree's size (`nodes_total`, `nodes_shown`, and `nodes_capped` when the node
bound cut this cohort's tree), a self-assessment (`calibration`), summaries
of its fatal forks and hotspots, and the tree under `root`. One node — the
search loop — with its children elided:

```json
{"abandoned":5,"children":[…],"ci95":[0.0435,0.2136],"cost_usd_sum":0.2,"depth":2,"duration_ns_sum":249849000000,"ended":0,"entropy":0.0,"example_runs":[{"outcome":"success","run":"search_loop-9"},{"outcome":"failure","run":"search_loop-0"},{"outcome":"abandoned","run":"search_loop-8"}],"failure":40,"fatal_fork":true,"fork":{"gap":0.6154,"p_value":2.2114329254814858e-14,"siblings_success_rate":0.7154,"siblings_visits":130},"hotspot":false,"lift":0.1837,"p_fail_given_here":0.8,"success":5,"success_rate":0.1,"token":"tool:search_orders×3-5","visits":50}
```

Per node:

| Field | Means |
|---|---|
| `visits`, `success`, `failure`, `abandoned` | Completed runs whose path passes through here, by outcome |
| `ended` | Runs whose path ends exactly here |
| `success_rate`, `ci95` | `success / visits`, with its 95% **Wilson** interval |
| `p_fail_given_here` | `failure / visits` |
| `lift` | `success_rate / base_rate`: 0.18 is "a fifth as likely to succeed as the agent's average run" |
| `entropy` | Bits of uncertainty in what runs did next, ending the run counted as one choice |
| `hotspot` | `entropy ≥ 1` bit, with `visits ≥ min_support` |
| `fatal_fork`, `fork` | See below |
| `cost_usd_sum`, `duration_ns_sum` | Summed over the runs through here (whole-run cost and duration, first span start to last span end) |
| `example_runs` | The newest run of each outcome through here — a success and a failure from the same point, ready for [`/v1/diff`](http-api.md#get-v1diff) |
| `omitted_children`, `omitted_visits` | Children not shown — below `min_support`, past `depth`, or past `max_nodes` — and their visits. Pruning is always reported |
| `cut` | Runs that went on past here along a branch the node bound kept from being built |

**Why Wilson.** Most branches are small. The textbook interval
`p ± 1.96·√(p(1−p)/n)` says 0 of 3 is "0%, give or take 0%"; Wilson's
interval says 0–56%, which is what three runs actually tell you. Every rate
foresight acts on is judged by its interval, not its point estimate.

**Fatal forks.** A child is a fatal fork when its success rate is at least
**25 points below its siblings' pooled rate**, both it and its siblings have
at least `min_support` runs, **and** a two-sided **Fisher exact test** puts
the difference at **p < 0.05**. The last condition is what keeps three
unlucky runs from being called a fork: the researcher's `query_db` branch
succeeded 3 of 10 times against 17 of 30 for its siblings — 27 points worse —
and is *not* flagged (p = 0.27); its `ask_human` branch, 1 of 10 against 19
of 30, is (p = 0.008). The `fork` object carries the comparison either way:
`siblings_visits`, `siblings_success_rate`, `gap`, `p_value`.

**Summaries.** `fatal_forks` lists the flagged forks within `depth`, most
significant first, each with its path from the root; `hotspots` the
highest-entropy nodes, with their most common next tokens:

```json
"fatal_forks":[{"fork":{"gap":0.6154,"p_value":2.2114329254814858e-14,"siblings_success_rate":0.7154,"siblings_visits":130},"path":["llm:gpt-4o","tool:search_orders×3-5"],"success_rate":0.1,"visits":50},{"fork":{"gap":0.4,"p_value":5.073367025895768e-05,"siblings_success_rate":0.9,"siblings_visits":60},"path":["llm:gpt-4o","tool:search_orders","llm:gpt-4o","tool:issue_refund×2"],"success_rate":0.5,"visits":30}],
"hotspots":[{"entropy":1.4955,"next":["tool:search_orders","tool:search_orders×3-5","tool:lookup_customer"],"path":["llm:gpt-4o"],"visits":180}]
```

### How good is this map? `calibration`

A prediction is only worth what it has been worth. Every map scores its own
foresight: each completed run is predicted at several depths, from the
**half of the cohort it is not in** (two-fold cross-validation, split by a
hash of the run id), and the predictions are scored against how the runs
ended. A run is predicted at depth *d* **only from what it had done by
then** — as live foresight would have seen it at the last step of its *d*-th
token, when that step might still repeat: from the runs of the other half
that reached the same point with the same step repeated at least as often
(the open group's pooling, [below](#foresight-for-one-run)), or by that
half's base rate when fewer than ten of them did:

```json
"calibration":{"depths":[{"baseline_brier":0.2514,"brier":0.2514,"covered":180,"depth":1,"runs":180,"skill":0.0},{"baseline_brier":0.2514,"brier":0.1998,"covered":180,"depth":2,"runs":180,"skill":0.2053},{"baseline_brier":0.2514,"brier":0.1727,"covered":180,"depth":3,"runs":180,"skill":0.3128},{"baseline_brier":0.2365,"brier":0.1923,"covered":130,"depth":5,"runs":130,"skill":0.1869}],"reliability":[{"high":0.1,"low":0.0,"observed":0.12,"predicted":0.08,"runs":25},{"high":0.2,"low":0.1,"observed":0.08,"predicted":0.12,"runs":25},{"high":0.7,"low":0.6,"observed":0.6,"predicted":0.6,"runs":40},{"high":0.8,"low":0.7,"observed":0.8222,"predicted":0.7111,"runs":45},{"high":0.9,"low":0.8,"observed":0.7111,"predicted":0.8222,"runs":45}]}
```

`brier` is the mean squared error of the predictions; `baseline_brier` the
same for the base rate alone; `skill = 1 − brier / baseline_brier` is how much
better than "assume the average" the map does. Here every run's first step is
the same, so depth 1 knows nothing (`skill` 0). The branch is chosen at depth
2, but only partly visible there: a run that has searched once cannot yet
tell a single search from the start of a loop, so the map removes 21% of the
base rate's error at depth 2 and 31% at depth 3, once the search is behind
the run. `reliability` bins depth-3 predictions in tenths: when the map said
about 10%, about 10% of those runs succeeded. **Read `skill` before trusting
a verdict**: an agent whose outcome does not depend on its path will show
`skill` near zero at every depth, and its foresight is then no better than
its base rate.

### Parameters

`depth` (default 12, at most 65), `min_support` (default 3: nodes with fewer
visits are pruned), `max_nodes` (default 500, at most 5,000: the most visited
nodes whose parents are shown), `task`, `since`/`until` (default: the last 30
days), `tenant`. An agent with no runs in the window is `404`.

## Foresight for one run

```sh
curl 'http://127.0.0.1:8080/v1/fate/runs/live-doomed-0'
```

```json
{"agent":"support-agent","base_rate":0.5444,"ci95":[0.0435,0.2136],"cohort_runs":180,"excluded_self":false,"explanation":"runs that took this path succeeded 5 of 50 times (10.0%, 95% CI 4.3–21.4%); the agent's overall success rate is 54%","last_open":true,"map_built_at_ns":1792303815044948000,"matched_depth":2,"min_support":30,"next":[{"ci95":[0.0435,0.2136],"success_rate":0.1,"token":"llm:gpt-4o","visits":50}],"node":{"abandoned":5,"ci95":[0.0435,0.2136],"cost_usd_avg":0.004,"depth":2,"failure":40,"lift":0.1837,"p_fail_given_here":0.8,"pooled":["tool:search_orders×3-5"],"success":5,"success_rate":0.1,"token":"tool:search_orders×3-5","visits":50},"outcome":"unknown","p_success":0.1,"partial":false,"path":["llm:gpt-4o","tool:search_orders×3-5"],"run":"live-doomed-0","steps":5,"support":50,"tenant":"","verdict":"likely_failure"}
```

The run's path is read from its stored spans exactly as a build reads runs,
and walked down its agent's map over the last 30 days. **A completed run is
left out of the statistics it is judged by** (`excluded_self: true`) — a
run's own outcome is not evidence about itself.

**A live run's last step is still open.** A run that may still go on has
not finished its last token: having issued one refund, it may yet issue a
second. Comparing it only with the runs that stopped at one refund would be
knowing what it does next, and would flatter it. So a live run is compared
with **every run that repeated its last step at least as often** at that
point — the sibling tokens of the same step in its own bucket and every
longer one, pooled (`node.pooled`, and `last_open: true`). The corpus's
healthy run has issued one refund:

```json
{"agent":"support-agent","base_rate":0.5444,"ci95":[0.6695,0.842],"cohort_runs":180,"excluded_self":false,"explanation":"runs that took this path succeeded 69 of 90 times (76.7%, 95% CI 66.9–84.2%); the agent's overall success rate is 54%","last_open":true,"map_built_at_ns":1792303815044948000,"matched_depth":4,"min_support":30,"next":[{"ci95":[0.7985,0.9534],"success_rate":0.9,"token":"llm:gpt-4o","visits":60},{"ci95":[0.3315,0.6685],"repeat":true,"success_rate":0.5,"token":"tool:issue_refund","visits":30}],"node":{"abandoned":0,"ci95":[0.6695,0.842],"cost_usd_avg":0.006,"depth":4,"failure":21,"lift":1.4082,"p_fail_given_here":0.2333,"pooled":["tool:issue_refund","tool:issue_refund×2"],"success":69,"success_rate":0.7667,"token":"tool:issue_refund","visits":90},"outcome":"unknown","p_success":0.7667,"partial":false,"path":["llm:gpt-4o","tool:search_orders","llm:gpt-4o","tool:issue_refund"],"recommended_next":"llm:gpt-4o","run":"live-healthy-0","steps":4,"support":90,"tenant":"","verdict":"on_track"}
```

69 of 90: the 54 of 60 that stopped at one refund and the 15 of 30 that
issued a second — not the 54 of 60 a look at how the run's refunds ended
would claim. `next` says what those runs did next: 60 went back to the model
(90% succeeded), 30 issued the refund again (`"repeat": true`, 50%) — so the
step that did best from here, `recommended_next`, is to move on. A completed
run (`last_open: false`) is judged at its own node. The verdict:

| Verdict | When |
|---|---|
| `novel` | The run took a step no earlier run of its agent took at that point. `divergence` says where: `depth` (tokens matched), `took`, and `common_next` — what the runs that got there did instead |
| `insufficient_history` | Fewer than `min_support` (default 30) earlier runs share the path; the agent has no completed runs in the window; or the map was cut short there by its node bound (so the step cannot be called novel either) |
| `likely_failure` | The interval's **upper** bound is below 50%: with 95% confidence, runs on this path fail more often than they succeed |
| `at_risk` | The upper bound is below the agent's base rate: runs on this path do measurably worse than the agent's average |
| `on_track` | Neither |

`p_success`, `ci95`, `support` and `node` describe where the path matched —
the deepest node, or a live run's pooled siblings; for `novel`, the point
where the run left the known paths. `next` lists what earlier runs did next
from there (options with at least three runs, most common first, each with
its rate and interval; for an open last group, the children of its own
bucket's node plus, marked `repeat`, the runs that made the same call
again), and `recommended_next` names the option whose runs did best — the
highest Wilson lower bound among options with at least `min_support` runs —
when it beats the current rate. From the corpus's novel run:

```json
{"divergence":{"common_next":["tool:search_orders","tool:search_orders×3-5","tool:lookup_customer"],"depth":1,"took":"tool:delete_database"},"explanation":"no earlier run of this agent took `tool:delete_database` at this point; the 180 runs that got here went on with `tool:search_orders`, `tool:search_orders×3-5`, `tool:lookup_customer`","recommended_next":"tool:search_orders","verdict":"novel", …}
```

`outcome` is how the run itself has ended so far (`unknown` while it is
live). `min_support=` sets the support a verdict needs. A run no span carries
is `404`.

### On the live view

[`GET /v1/runs/live`](http-api.md#get-v1runslive), its stream and the MCP
`list_live_runs` tool carry each running run's foresight — verdict, success
rate, interval and support — read from the run guard's memory and the cached
map, so a dashboard can rank what is running now by how runs like it have
ended. See [steer](steer.md#foresight-on-live-runs) for how it is kept
current.

## Predictive fuses

A [fuse policy](guard.md#policies) may set three more limits:

| Limit | Default | Means |
|---|---|---|
| `min_success_odds` | — | The success odds a run's path must keep, in `(0, 1]` |
| `foresight_min_support` | 30 | Earlier runs that must share the path before the limit may act |
| `foresight_after_steps` | 3 | Recorded steps the run must have before the limit may act |

When a run's foresight has at least that support, the run has at least that
many steps, and the **upper bound** of the 95% interval is below
`min_success_odds`, the policy's action applies — `stop` or `warn` — with the
reason:

```sh
curl -X POST http://127.0.0.1:8080/v1/fuses/policies -d '{
  "name": "support", "match": {"service": "support-agent"},
  "limits": {"min_success_odds": 0.7}, "action": "stop"}'
curl -X POST http://127.0.0.1:8080/v1/fuses/check -d '{"run": "live-doomed-0"}'
```

```json
{"evidence":[],"foresight":{"base_rate":0.5444,"ci95":[0.0435,0.2136],"explanation":"runs that took this path succeeded 5 of 50 times (10.0%, 95% CI 4.3–21.4%); the agent's overall success rate is 54%","map_built_at_ns":1792303815044948000,"matched_depth":2,"p_success":0.1,"raised":true,"successes":5,"support":50,"verdict":"likely_failure"},"grant":null,"limits":{"min_success_odds":0.7},"policy":"support","reasons":["foresight: runs that took this path succeeded 5 of 50 times (≤21.4%)"],"run":"live-doomed-0","source":"rebuilt","tenant":"","tripped_at_ns":null,"tripped_by":null,"usage":{"cost_usd":0.002,"duration_ns":4326000000,"error_streak":0,"errors":3,"llm_calls":1,"steps":5,"tokens":690,"tool_calls":4,"top_repeat":null},"verdict":"stop"}
```

The upper bound, not the estimate, is compared, so a fuse acts only when the
history is strong enough to be confident — 0 of 3 runs never stops anything.
The fuse reads the run as still going (the check is about its next step), so
its last step is always open, as above.

**The step about to happen.** When the check names the next step's tool
(`"next": {"tool": "issue_refund"}`), the path is judged *with that step
appended* — one more of the last token's step, or a new open token — if
earlier runs took it from here. So the corpus's healthy run (69 of 90, upper
bound 84.2%) answers `continue` under `min_success_odds: 0.7`; the same check
naming a second `issue_refund` answers `stop`:

```json
{"evidence":[],"foresight":{"base_rate":0.5444,"ci95":[0.3315,0.6685],"explanation":"if it calls tool:issue_refund next: runs that took this path succeeded 15 of 30 times (50.0%, 95% CI 33.2–66.8%); the agent's overall success rate is 54%","map_built_at_ns":1792303815044948000,"matched_depth":4,"next_tool":"issue_refund","p_success":0.5,"raised":true,"successes":15,"support":30,"verdict":"on_track"},"grant":null,"limits":{"min_success_odds":0.7},"policy":"support","reasons":["foresight: runs that took this path and then called tool:issue_refund succeeded 15 of 30 times (≤66.8%)"],"run":"live-healthy-0","source":"live","tenant":"","tripped_at_ns":null,"tripped_by":null,"usage":{"cost_usd":0.004,"duration_ns":4266000000,"error_streak":0,"errors":0,"llm_calls":2,"steps":4,"tokens":1460,"tool_calls":2,"top_repeat":{"count":2,"example":{"span_id":"000000000000df03","trace_id":"cbf29ce48422232500000000000000df"},"fingerprint":"llm:gpt-4o"}},"verdict":"stop"}
```

(The foresight `verdict` is `on_track` — 66.8% is above the agent's 54% —
while the policy's odds of 0.7 still raise its action: the verdict compares
with the agent, the limit with the policy.) A step no earlier run took adds
no evidence, and the path so far is judged instead. An LLM step's model is
not part of the check, so only a tool can be judged ahead.

**Where it is evaluated, and where it is not.** On the pre-call check
(`POST /v1/fuses/check`) and on verdict queries (`GET /v1/fuses/runs/{run}`,
MCP `check_run_guard`) — never on ingest, so an acknowledgement never carries
a foresight stop, and not in the `GET /v1/fuses/runs` listing, which is
answered from memory alone. A run already stopped is not foreseen.

**From memory, against a cached map.** The run guard keeps, for every run
under a policy that sets `min_success_odds`, the run's behaviour path — its
earliest 192 steps, by key — beside its usage, maintained as usage is: every
version of a span that lands replaces the last, a rebuild from the store
replays it whole, and eviction or erasure takes it away. The check reads
that path from memory and walks it down a cached map: time proportional to
the path, never a read of the run from the store. (A run whose guard state
started before such a policy existed, or that the guard does not hold whole,
is read from the store instead, under what is left of the request's
`--query-deadline-ms` budget.) The check never builds a map. With none
cached, it answers `"foresight": {"verdict": "insufficient_history", …}` (no
stop) and asks for one; `traza-server` builds requested maps on a background
thread every two seconds, two at a time, each under `--fate-build-seconds`
(default 120, never less than the query deadline), and a map is served to
the fuses for up to twice `--fate-cache-seconds` while a fresh one is built.
So a server's first checks after start see no foresight for a few seconds. A
background build that runs out of time is retried, after a minute (or a cache
lifetime), over **half** its window — down to under two days — and widened
again a step once a build finishes with three quarters of its budget to
spare (until then the tenant's default-window map and run routes answer
over the same narrower window, with `bounds.window_narrowed`); meanwhile the
fuse's foresight says why it has none:
`"foresight is unavailable: the last background build of this tenant's Fate
Map ran out of time; it is retried over a shorter window, the last 15.0
days"`. Every build is counted in `traza_fate_builds_total{result="ok"|"failed"}`.
If the run cannot be read, the answer is likewise no foresight stop, never a
refusal: missing evidence is not evidence of failure.

**Not latched.** A foresight stop is a prediction, made again at every
question: it has no `tripped_by`, is not announced on the event feed, and
lifts by itself if the run's path moves on to a node whose odds clear the
limit. Reset does not clear it (the next question makes it again); to let a
run go on against its history, reset it with a grant of lower odds:
`{"grant": {"min_success_odds": 0.05}}`.

## Bounds, cache and erasure

A build reads every span in its window — the cost of any whole-window
aggregation. A request's build runs under `--query-deadline-ms`; a window too
large to read in time answers `400` and the advice to narrow it with
`since=`/`until=`. A background build for the fuses runs under
`--fate-build-seconds` and narrows itself when it runs out (above).
Everything a build holds is bounded, and a build that meets a bound says so
(`truncated: true`, and which in `bounds`):

| Bound | Default | Past it |
|---|---|---|
| Completed runs per cohort map (`--fate-max-runs`) | 20,000 | The oldest are dropped (`runs_dropped`) |
| Runs the fold tracks per build, across every agent of the build (each holding its run id, at most 1 KiB, and a fixed-size record) | 4 × `--fate-max-runs` | The window is **narrowed** to its most recent part that fits (`window_narrowed`, `window.since_ns`), so a map keeps the newest runs, not whichever the fold met first; past four narrowing attempts, further runs are skipped (`spans_skipped`). The bound is the build's, not an agent's: one busy agent narrows the window for every agent of its tenant |
| Step records held per build (32 bytes each) | 2,097,152 (64 MiB) | As for runs |
| Steps read per run | 192, the earliest | The path ends in `…` (`runs_partial`) |
| Tree nodes per build, across its cohorts (`--fate-max-nodes`) | 50,000 | Nodes are created **level by level across every cohort** — each depth, most visited first, before any cohort's next depth — so the bound cuts the long tail of deep, rare paths and no busy agent can leave another agent of the tenant a bare root (`node_cap` for the build, `nodes_capped` for a cohort it cut, and `cut` on the nodes it cut below). A path into a cut branch — or a live run whose last step has a repeat bucket the bound cut there, so its pool would be short — gets `insufficient_history`, never a guess |
| Distinct step signatures, and distinct path tokens, per build | 20,000 each | The path ends in `…` before the first one past it (`signature_cap`) |
| Bytes of a name kept (tool, model, agent, service) | 160 | Cut, ending in `…` |
| Cohorts per build | 256, the largest | Dropped (`cohorts_dropped`) |
| Tasks listed per cohort | 32, with at least 3 runs | Not listed |
| Distinct agent and service names per build | 4,096 | The spans are skipped (`spans_skipped`) |
| Builds running at once, across keys | 2 | A third waits, under its own deadline, for one to finish |
| Cached tenant maps — each tenant's default-window build, the one its fuses read (`--fate-tenant-maps`) | 32 | The least recently used tenant's is rebuilt when next asked for. No other request can evict from this pool |
| Other cached builds — a task, an explicit window, the operator view | 8 | Least recently used evicted, among these only |
| Behaviour path a fuse keeps per live run (48 bytes a step) | 192 steps, the earliest | The path past them is unknown and reads as partial; the steps count against the run guard's span budget ([guard](guard.md)) |

Builds are cached for `--fate-cache-seconds` (default 300) per tenant,
window and task, and built once however many requests ask at the same
moment. `0` disables the cache — every request builds, and the predictive
fuses, which read only cached maps, never act. **Any erasure drops every
cached map** (at its start and again when its purge completes), and a build
that began before an erasure is not cached afterwards: a map must not keep
showing a run the store no longer holds.

## Tenancy

Cohorts are per tenant, and a build never mixes tenants: the operator view
(an unbound credential naming no tenant on `/v1/fate/cohorts`) lists every
tenant's cohorts, each with its `tenant`. A bound credential reads only its
own tenant — naming another is `403 {"required": "unbound"}` — and another
tenant's run is `404`. An unbound caller naming no tenant addresses the
default tenant on the map and run routes.

## Limitations

Stated plainly:

- **Correlation, not cause.** A fatal fork says runs that went that way
  ended worse. The step may be a symptom (the agent searched again *because*
  the order did not exist) rather than the cause; the fork tells you where to
  look, and `example_runs` give you a pair to diff.
- **Paths ignore inputs.** Two runs that called the same tools in the same
  order are the same path, whatever their arguments or prompts. A behaviour
  that differs only in content is invisible to the map.
- **Outcomes are inferred.** Without a declared `session.outcome`, a run's
  outcome is the status of the span that ended last — a run that failed but
  ended on a successful span reads as a success. Declare outcomes where you
  can.
- **Many nodes, some luck.** Each fork test is at p < 0.05 with no correction
  for how many nodes were tested, so in a large tree an occasional fork is
  chance; the 25-point gap and the support rule make that rarer, and every
  fork carries its `p_value` so you can hold it to a stricter bar.
- **The cohort is the agent, whatever changed.** A new prompt, model or tool
  catalog mid-window mixes two behaviours in one map. Narrow the window to
  after the change.
- **The window's edge.** A run that began before the window starts is read
  from its spans inside the window, so its path starts mid-run; with a
  30-day window that is a sliver of runs, but a narrowed window has more of
  them.
- **Step order on ties.** Steps are ordered by start time, then by the first
  eight bytes of their trace and span ids — the canonical order on everything
  but a tie of start time and both prefixes. Likewise the span that ended
  last (which decides an undeclared outcome) is chosen by end time and the
  ids' first eight bytes, then by a hash of both ids.
- **Buckets blur an open group.** A live run's last step is pooled with
  every bucket that can hold its count, but a bucket does not keep exact
  counts: a run on its fourth search is pooled with the `×3-5` runs that
  stopped at three, as well as those that went on.
- **The fuse reads every run as live.** It is asked about a next step, so
  even a run that has ended has its last step pooled; `/v1/fate/runs/{run}`
  judges a completed run at its own node.
- **The run-tracking bound is the build's.** One busy agent that fills it
  narrows the window for every agent of its tenant (`window_narrowed` says
  so); a quiet agent's map then holds fewer runs than the full window would.
- **A run older than its predictive policy** keeps no behaviour path in the
  guard until the guard next rebuilds it (its first verdict query or check
  does), and is read from the store meanwhile.
- **Background builds wait their turn.** The warmer builds two requested
  maps per two-second tick, one after another, each up to
  `--fate-build-seconds`; a slow tenant's build delays the next tenant's.
- **Calibration is noisier than the map.** It predicts each run from half
  the cohort, so a small cohort's `skill` moves with the split.
- **A completed run straddling a rebuild.** Leave-one-out takes out what a
  run contributed when the map was built; a run that gained spans since is
  judged on its current path against its old contribution.
- **One process.** Maps are cached per server; replicas build their own.
