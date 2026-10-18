# Change Radar: what changed?

"It worked last week" is the most common sentence in agent operations and the
hardest to act on. Somebody edited the system prompt. A tool was added. The
provider rolled a new snapshot behind the same model alias. A deploy bumped
three things at once. Which of them, if any, is why the error rate doubled?

Traza keeps every run an agent made, so it can answer from evidence.
[`GET /v1/changes`](http-api.md#get-v1changes) (and the MCP tool
[`what_changed`](mcp.md)) reads a window of an agent's runs and returns a
**ranked timeline of change points**. Each one says what changed and when,
how many runs ran on each side, how error rate, success rate, cost, steps,
duration and tool mix moved (with p-values), and a **verdict**: `regression`,
`improvement`, `neutral` or `insufficient_data`. Regressions come first.

Everything here is deterministic arithmetic over stored spans. Nothing calls a
model, nothing is learned, nothing is stored. Each request computes its answer
from the spans as they are now, so an erasure applies to the next answer.

## Quick start

Send your spans as usual. With OpenTelemetry, set `service.version` on the
resource, for example with `OTEL_RESOURCE_ATTRIBUTES=service.version=1.5.0`,
which OpenTelemetry SDKs read. Then ask:

```sh
curl 'http://127.0.0.1:8080/v1/changes?agent=support-bot'
```

The example below is real output, trimmed to the top change. A support agent
ran 60 times on version 1.4.2. Version 1.5.0 shipped with a one-word prompt
edit, and two runs in five started failing where one in twenty had:

```json
{
  "agents": [
    {
      "agent": "support-bot",
      "changes": [
        {
          "at_ns": 1792135560000000000,
          "behavior_shift": {"js_distance": 0.0, "p_value": 1.0},
          "co_changes": ["service_version"],
          "decided_by": "quality",
          "deltas": {
            "error_rate": {"after": 0.4, "before": 0.05, "n_after": 60, "n_before": 60, "p_value": 4.4159082510402535e-06},
            "success_rate": {"after": 0.6, "before": 0.95, "n_after": 60, "n_before": 60, "p_value": 4.4159082510402535e-06},
            "cost_per_run": {"after": 0.008000000000000005, "before": 0.008000000000000005, "n_after": 60, "n_before": 60, "p_value": 1.0, "ratio": 1.0},
            "…": "steps_per_run, duration_ns_per_run and tool_mix trimmed"
          },
          "detail": {
            "after_chars": 62,
            "before_chars": 63,
            "diff": {"after": "Never", "before": "Always", "context": "triage support tickets. ", "offset": 28}
          },
          "dimension": "system_prompt",
          "effect_size": 0.9184115942083035,
          "example_runs": {
            "after": ["ticket-060", "ticket-061", "ticket-062"],
            "before": ["ticket-059", "ticket-058", "ticket-057"]
          },
          "explanation": "error rate 5.0% → 40.0% (p<0.001); success rate 95.0% → 60.0% (p<0.001)",
          "from": "b9a28b7198c279cb",
          "runs_after": 60,
          "runs_before": 60,
          "to": "118dd41ec67c4887",
          "verdict": "regression"
        }
      ],
      "current": {
        "model_requested": "gpt-4o",
        "model_returned": "gpt-4o-2024-08-06",
        "sdk_version": "opentelemetry/1.27.0",
        "service_version": "1.5.0",
        "system_prompt": "118dd41ec67c4887"
      },
      "first_run_ns": 1792131960000000000,
      "last_run_ns": 1792139100000000000,
      "runs": 120,
      "summary": "support-bot: regression at 2026-10-16T07:26:00Z: system prompt changed (b9a28b7198c279cb → 118dd41ec67c4887), together with the service version; error rate 5.0% → 40.0% (p<0.001); success rate 95.0% → 60.0% (p<0.001).",
      "tenant": "",
      "truncated": false,
      "unnamed": false,
      "volatile": []
    }
  ],
  "runs_scanned": 120,
  "since_ns": 1789712780521414000,
  "spans_scanned": 360,
  "truncated": false,
  "until_ns": 1792304780521414000
}
```

The deploy and the prompt edit are both change points at the same run
(`co_changes`). The radar cannot tell which of the two broke the agent, so it
names both. `detail.diff` shows the edit itself. `example_runs` names the
last run before and the first run after, which you can open with
[`GET /v1/tapes/{run}`](http-api.md#get-v1tapesrun) or compare with
[`GET /v1/diff`](http-api.md#get-v1diff).

## One run on the timeline

When you are looking at one failed run, add `run=` to ask which of its
agent's changes it ran downstream of. The analysis narrows to that run's
agent, and the answer gains a `run` object. This is real output for a run of
the example above:

```sh
curl 'http://127.0.0.1:8080/v1/changes?run=ticket-076'
```

```json
{"agent":"support-bot","downstream_of":[{"at_ns":1792135560000000000,"dimension":"system_prompt","explanation":"error rate 5.0% → 40.0% (p<0.001); success rate 95.0% → 60.0% (p<0.001)","to":"118dd41ec67c4887","verdict":"regression"},{"at_ns":1792135560000000000,"dimension":"service_version","explanation":"error rate 5.0% → 40.0% (p<0.001); success rate 95.0% → 60.0% (p<0.001)","to":"1.5.0","verdict":"regression"}],"errored":true,"fingerprint":{"model_requested":"gpt-4o","model_returned":"gpt-4o-2024-08-06","path":"llm:gpt-4o-2024-08-06 > tool:lookup_order > llm:gpt-4o-2024-08-06","sdk_version":"opentelemetry/1.27.0","service_version":"1.5.0","system_prompt":"118dd41ec67c4887"},"outcome":"failure","run":"ticket-076","start_ns":1792136520000000000,"tenant":""}
```

`fingerprint` is the run's value for each dimension it carries, plus its
top-level `path`. `downstream_of` lists, for each dimension, the latest
change point at or before the run's start whose new value the run carries,
regressions first. A run that still carries an old value (a straggler on the
previous version) is not downstream of that change. For a `behavior` change,
any run that started after it counts as downstream. `downstream_of` is empty
when the run used values older than every change in the window.

A run that is not among the window's agent runs gets `404`. A run that is,
but that `agent=` or `environment=` excludes, gets `400` saying which filter
excluded it, so a typo in a filter does not read as a missing run. A run id
used in two tenants needs `tenant=` from an unbound caller (the MCP tool
takes `tenant` too), or the request is `400`.

## What counts as a change

Each run gets a **configuration fingerprint**, computed from its spans on
every read. A run that carries several values of one dimension (a planner
model and an executor model, say) has the set of them as its value, shown
joined with ` + `.

A run holds at most four distinct values of a dimension. A run that carried
more has an **incomplete** set: which values it ran on is not fully known,
so it is left out of that dimension's change detection and of the sides of
its changes, as a run that carries none of the dimension is. Which four it
holds depends only on the set, never on the order its spans were read, and
it is shown as those four and ` + …`. Holding the first four read instead
would make two runs on the same five models look like different
configurations whenever their spans arrived in a different order, and would
miss an edit to the fifth.

| Dimension | Read from (first present wins) | Shown as |
|---|---|---|
| `system_prompt` | The request's `system` and `developer` messages, with `gen_ai.system_instructions` folded in (the [step request](../llm-semantics.md#input-and-output) precedence: `gen_ai.input.messages`, indexed `gen_ai.prompt.{i}.*`, OpenInference `llm.input_messages.*`, …). When the request is a provider body (OpenInference `input.value`), its top-level `system` and its `messages` | 16 hex characters of SHA-256 over the text, volatile tokens masked |
| `tool_catalog` | `gen_ai.tool.definitions`, else indexed `llm.request.functions.{i}.*` (OpenLLMetry), else indexed `llm.tools.{i}.tool.json_schema` (OpenInference), else a provider request body's `tools` | 16 hex characters of SHA-256 over the definitions' canonical JSON, sorted, so their order does not matter |
| `model_requested` | `gen_ai.request.model`, else `llm.model`, else `llm.model_name` | the value |
| `model_returned` | `gen_ai.response.model` | the value |
| `params` | Every `gen_ai.request.*` key except the model, plus `llm.invocation_parameters` without its `model` key. These are the keys a [run diff](http-api.md#get-v1diff) calls `config` | `key=value` pairs in key order, each value as JSON (strings quoted), for example `max_tokens=1024, stop="END"` |
| `service_version` | `service.version`, on any span of the run | the value |
| `sdk_version` | `telemetry.sdk.name` and `telemetry.sdk.version`, on any span | `name/version` |
| `behavior` | The run's top-level path (see below), when no configuration change explains a shift | the most common path on each side |

The first five are read from model-call steps. A value longer than 160
characters is identified by its digest and shown cut short. Sampling
parameters are identified by the canonical JSON of the typed parameters
(keys sorted, strings quoted and escaped), never by the text shown: a `stop`
string that spells out `END, temperature=0.7` is a different configuration
from a `stop` of `END` beside a `temperature` of 0.7, and `1` differs from
`"1"`.

Runs are compared by a 64-bit digest of each value, never by its text, so
the number of distinct values cannot hide a change. The text is kept only to
be shown, in bounded tables (see [Bounds and cost](#bounds-and-cost)). A
value whose name did not fit is shown as `#` and its 16 hex digits, and its
agent says `"unnamed": true`.

Some edge cases:

- A request with **no system message** has the system prompt `(none)`, so
  removing a system prompt is a change. An empty tool list is `(none)` too.
- A request that **was not captured**, or that ingest **offloaded whole** to
  the payload store, has no system-prompt value. Reading it would cost a
  payload read per step, and guessing would be worse than leaving it out.
  When only `gen_ai.system_instructions` was offloaded, its content address
  identifies it.
- **Volatile tokens** are masked as `<v>` before the system prompt is
  digested. Without that, a prompt that embeds today's date or a request id
  would be unique on every run, and the edit someone actually made would be
  lost in the noise. The masked tokens are UUIDs, ISO dates (with an
  optional time), clock times, runs of six or more digits, and runs of
  sixteen or more hex characters, each only as a whole token. Short numbers
  are kept, so `policy v2` → `policy v3` is still a change.
- A dimension where more than half of at least 20 runs carry a value that no
  other run shares is **volatile**: it is listed in the agent's `volatile`
  array and not searched. This usually means a prompt embeds something per
  request that the masking above does not catch. A dimension where more than
  half of the runs carrying it have an incomplete set (above) is listed as
  volatile too, and so is a dimension with more than 100 change points in
  the window: a value that changes that often is not a history of deploys.

### Agents

Runs are grouped by `(tenant, agent)`. The agent is the first step's agent
name (`gen_ai.agent.name`, Traceloop's agent entity, OpenInference
`agent.name`), or else the service of the run's earliest span. This is the
same cohort rule the Fate Map uses. A run is one
[run](../llm-semantics.md#run-identity): a session id, or
`trace:<trace_id>`. Only **agent runs** are analysed, meaning runs with at
least one llm, tool or agent step. A plain HTTP trace stored beside them is
not an agent and does not appear.

### Getting the build attributes in

Over OTLP, the resource attributes `service.version`,
`deployment.environment.name`, `deployment.environment`, `telemetry.sdk.name`
and `telemetry.sdk.version` are copied onto every span of their resource as
span attributes. A span that already has one of these keys keeps its own
value. All other resource attributes are still dropped (see
[ingest](ingest.md#which-resource-attributes-reach-the-span)). Over the
native `POST /v1/spans`, put the same keys in the span's `attributes`.

`deployment.environment(.name)` is not a dimension. It is a filter:
`?environment=prod` analyses only runs that report it, so a staging deploy
does not blur production's timeline.

## How change points are found

For each dimension, the agent's runs are walked in start order through a
sliding window of `clamp(runs/8, 8, 50)` runs that carry a value of that
dimension. Runs that carry no value for a dimension are skipped for it.
Finding a change takes two steps: the walk **proposes**, and runs the walk
did not use **confirm**.

The walk proposes a change point when a value becomes **dominant**:

- A value becomes dominant when it holds at least half of the window and
  leads the current value by `max(2, window/10)` runs.
- The proposal is placed at the first new-value run of the split that best
  separates old from new inside that window. For a clean deploy this is
  exactly the first run on the new value. For a ramped rollout it is where
  the new value took over.
- A value that **never becomes dominant** is never proposed. A 5% canary
  and a one-off fall in this group.
- A newcomer that **loses dominance again within one window** was an
  excursion, and its proposal is withdrawn.
- While the first value has fewer than three runs, the dimension is still
  **settling**. A takeover then is not proposed.

A value drawn at random per run, such as an A/B split, a router's two
models, or a branch that sometimes calls a guardrail model, wins a window by
chance all the time. So a proposal becomes a change point only when it is
**confirmed** by the window of runs *after* the newcomer became dominant,
which played no part in proposing it:

- after: the old value holds at most a fifth of those runs, and the new value
  holds at least half;
- before: over at least half a window before the onset, the new value was
  rare (at most a fifth), or the old value was established (at least three
  fifths), or, when the window is at least 24 runs, the old value's share
  fell by at least 0.3 across the change. The last case is a rollout that
  held a mixed stage and then completed.

In simulations of this rule on random sequences that never change (300
sequences each of 50/50, 70/30, 60/40 and three-way splits, 1,000 to 5,000
runs long), at most 1 sequence in 100 showed a change point. Short cohorts
are noisier, because their window is short: about 1 in 20 for an even split
over 100 runs, and 1 in 60 over 150. A real rollout that holds a mixed stage
for a long time is found when it completes, in about half the simulated
cases; the rest stay a mixture to the radar.

Two more rules cover the edges of the window:

- A proposal too recent to have a full window after it is confirmed only if
  the new value was rare before it, holds the majority since, and the old
  value is rare since.
- A **fresh value** at the end of the window is a change point once it has
  three runs, even before it fills half a window, provided it has held
  every run since it appeared and was rare before. This is how a deploy
  from an hour ago shows up, usually as `insufficient_data`. The agent's
  `summary` names it ("Since then: … (too early to judge)") when a more
  significant change heads the ranking.

### Behaviour changes

Sometimes the agent behaves differently with no configuration change at all,
because the provider changed under you. To catch this, each run gets a
**top-level path**: its first four step tokens (`kind:name`, as a
[tape](replay.md) signature has them), with consecutive repeats bucketed `x`,
`x×2`, `x×3-5` and `x×6+`, and ` > …` when more steps follow. For example:
`llm:gpt-4o > tool:search×2 > llm:gpt-4o > …`.

Path distributions in adjacent windows are compared by **Jensen–Shannon
distance** (base 2, from 0 for identical distributions to 1 for disjoint
ones). Scanning every split for the largest distance and then testing that
split as if it had been chosen in advance would call noise significant most
of the time, so the runs are split in two:

1. The **even-indexed runs choose**. Over windows of `clamp(n/6, 15, 100)`
   of them (`n` the even runs), each stretch of splits whose distance is at
   or above the threshold (`behavior_threshold`, default 0.3, roughly a sixth
   of runs moving to a new path) gives one candidate, its peak. Candidates are
   taken strongest first, at least a window apart, none within a window of a
   configuration change point, at most 16.
2. The **odd-indexed runs test**. They took no part in choosing, so a test on
   them is honest. Their windows either side of the candidate must also reach
   the threshold, and a **permutation test** (1,999 relabellings, seeded from
   the agent's name so the same data gives the same p-value every time) must
   give a p-value below 0.01 after Bonferroni's correction for the number of
   candidates tested.

Under the independence the statistics assume, this holds the chance of
reporting any behaviour change in a cohort whose paths never changed at 1%.
The reported `behavior_shift` is the odd half's distance and corrected
p-value. Behaviour detection needs at least 60 runs (two windows of 15 in
each half), and the split costs power. In simulations, a total switch of
paths was found in every cohort of 120 runs, and a new path taking 30% of
runs in 28 of 30 cohorts of 2,000 runs, but a shift from 70/30 to 30/70
between two paths in only 5 of 30 cohorts of 200 runs.

When both sides of a reported configuration change have at least 10 runs,
the change also carries `behavior_shift`: the distance and p-value (199
relabellings) between its two sides, at most 200 runs each. Its split is the
change's own, chosen by the configuration and not by the paths, so it needs
no correction. This shows whether the agent took different paths after the
prompt changed.

## Deltas and verdicts

Each change compares two sides:

- **before**: runs on the old value, between the previous change point and
  this one;
- **after**: runs on the new value, between this change point and the next.

Change points of other dimensions within two runs of this one are
co-changes. They are listed in `co_changes` and do not cut the sides. Each
side is capped at the 2,000 runs nearest the change. **Runs that may still
be going** are left out, because their cost and steps may be partial: runs
with no declared outcome whose last span ended within the 15-minute
session-idle window of *now*, or of `until` when `until` is in the past (a
run cut off by the end of the window may have gone on after it). For a
`behavior` change, the sides are all runs between the neighbouring change
points.

| Delta | Per run | Test |
|---|---|---|
| `error_rate` | at least one span with status `error` | pooled two-proportion z-test, or Fisher's exact test when the table is sparse |
| `success_rate` | ended `success`, of the runs that ended at all ([outcome](../llm-semantics.md#session-outcome-and-goal--traza-extensions): declared, else the last span to end) | pooled two-proportion z-test, or Fisher's exact test when the table is sparse |
| `cost_per_run` | cost in USD, metered or [priced](../configuration.md#model-pricing) | Welch's t-test on `ln(cost)` over priced runs |
| `steps_per_run` | llm, tool and agent spans | Welch's t-test on `ln(1 + steps)` |
| `duration_ns_per_run` | first start to last end | Welch's t-test on `ln(1 + milliseconds)` |
| `tool_mix` | each tool's share of tool calls | shown, not tested |

`before` and `after` are proportions (0 to 1) or arithmetic means. `ratio`
is `after / before`. Each delta also reports how many runs it counted on
each side (`n_before`, `n_after`). The mean tests run on a log scale, so a
single runaway run cannot decide a verdict. When no run on either side is
priced, the cost delta is reported empty rather than as a confident zero.

A rate's z-test leans on the normal approximation, which overstates
significance when the table is sparse: one success of one run before
against none of ten after is p = 0.0009 by the z-test, and 1/11 exactly.
So when any expected cell count of a rate's 2×2 table is below 5 (Cochran's
rule), the rate is tested by Fisher's exact test instead. The p-values come
from in-module implementations of the normal and Student-t distributions and
of Fisher's test, checked in tests against SciPy reference values.

A metric takes part in the verdict only when **its own test** had at least
10 observations on each side. For error rate, steps and duration that is
every finished run. For success rate it is only the runs that ended (a run
that declared an outcome Traza does not recognize, such as `escalated`, did
not end in success or failure), and for cost only the priced runs; sixty
finished runs a side can still hold a single rated one. A metric short of
the bar is still reported, with its counts (`n_before`, `n_after`) and
p-value, for you to weigh.

Holm's correction holds the family-wise error rate at 5% across a change's
tests: up to five, since a test is left out when it cannot be computed (no
priced runs, or no variation at all) or is short of the bar above. The
verdict follows these rules, in order:

1. `insufficient_data`: fewer than 10 finished runs on either side. The
   deltas are still reported for you to weigh.
2. `regression`: a **quality** metric (error rate or success rate) got
   significantly worse.
3. `improvement`: a quality metric got significantly better.
4. If quality held: `regression` when an **efficiency** metric (cost, steps,
   duration) got significantly worse, `improvement` when one got
   significantly better.
5. `neutral` otherwise.

`decided_by` says which kind of metric decided a `regression` or
`improvement`: `quality` (rule 2 or 3) or `efficiency` (rule 4). It is absent
for `neutral` and `insufficient_data`. `explanation` lists the metrics that
decided the verdict, with any that moved the other way ("— though: …", "— at
a price: …"). `effect_size` is the largest absolute effect among them:
Cohen's h for rates, and Cohen's d for log-scale means, capped at 4 (past
that the two sides barely overlap). A difference every run shows, with no
spread on either side (every run took 3 steps, then every run took 4), is
reported at the cap. Equality and "no spread" are judged with a relative
tolerance of 10⁻¹², so floating-point rounding is never read as a difference.

The two effect sizes are on different scales, so they are only compared
within one kind of verdict. Changes are ranked by verdict (regressions
first), then quality-decided before efficiency-decided, then by effect size,
then newest first. A one-step efficiency regression therefore never outranks
an error-rate regression, in the ranking or in the agent's headline.

Each change also carries:

- `detail`, which depends on the dimension:
  - for a **system prompt**, the first differing stretch of the masked text
    (≤ 80 characters a side, with a little context and the character offset).
    When the run carries a set of prompts (a planner and an executor, say)
    and exactly one of them changed, the diff is of that one;
  - for a **tool catalog**, the tools added and removed, or
    `definitions_changed` when the names are the same;
  - for **sampling parameters**, the keys that differ, with their values
    as typed JSON (a long value cut short);
  - for **behaviour**, the five paths whose share moved most.
- `example_runs`, the three runs nearest the change on each side.

Each agent also has `current` (the dominant value of each dimension over its
most recent runs) and `summary`, a one-sentence headline.

## Reading the answer

`GET /v1/changes` answers
`{"since_ns", "until_ns", "covered_since_ns"?, "spans_scanned", "runs_scanned",
"agents": [...], "run"?, "truncated"}`. Each agent is
`{"agent", "tenant", "runs", "first_run_ns", "last_run_ns", "summary",
"current", "volatile", "changes": [...], "truncated", "unnamed"}`. The
[HTTP reference](http-api.md#get-v1changes) has every field.

## Bounds and cost

| Bound | Value |
|---|---|
| Window | `since`/`until`. The default is the 30 days up to now |
| Runs analysed | `--radar-max-runs` (default 20,000): the newest agent runs, by the start of their first step. Past it, `truncated: true` and `covered_since_ns` say where the analysed window starts. Every agent run whose first step starts at or after that point is read in full |
| Memory per run held | Typically about a kilobyte; at most about 6 KB. Ids are clipped to 128 bytes, names to 256, declared outcomes to 32, and lists are capped as below |
| Earliest steps kept per run | 32 (enough for the top-level path) |
| Values per dimension per run | 4. A run that carried more is shown with `+ …` and left out of that dimension's change detection |
| Tool counts per run | 64 tools |
| Display names | 2,048 values per dimension, at most 256 of them from one tenant's service; 8,192 step and tool names, at most 1,024 per service. A value past these is still compared exactly, and shown as `#` and its digest (`unnamed: true`) |
| Diff samples | 1 MiB per dimension (prompt texts up to 4 KB each, catalog tool names, parameters) |
| Change points per dimension | 100 (past it, the dimension is reported as `volatile`) |
| Agents | 100, the busiest (then `truncated: true`) |
| Changes per agent | 40, after ranking (then the agent's `truncated: true`) |
| Behaviour candidates tested per agent | 16, the strongest distance peaks. Each costs one permutation test of up to 1,999 relabellings of at most 200 runs, which stops as soon as the candidate cannot be significant |
| Path-shift tests | One 199-relabelling test of at most 400 runs for each of the at most 40 configuration changes reported, and none for the changes ranked out |
| Compute | Two passes over one snapshot of the window, then the analysis, all under one [`--query-deadline-ms`](../configuration.md#server-flags) budget. An analysis that runs out of budget is refused with `400` and no partial answer, and counted in `traza_query_deadline_exceeded_total` |

With the default cap, one request holds at most about 120 MB of runs, plus
the name tables, path names and the first pass's index: under 200 MB in the
worst case, and a few tens of MB for typical runs.

**The run cap is exact, not approximate.** The read makes two passes over
one snapshot of the window:

1. The first pass reads only step spans (llm, tool and agent). For each run
   it notes when its first step started, and it keeps the newest runs by that
   time. Whenever more than twice the cap are noted, everything but the
   newest `--radar-max-runs` is let go, and the cut moves to the latest
   first-step start let go. Plain request traces have no step, so this pass
   never notes them: they cannot crowd agent runs out, and no store is too
   big to ask about.
2. The second pass reads every span of exactly the runs the first pass kept,
   including a run's workflow root, its HTTP server span and any span that
   started before its first step.

So every agent run whose first step starts at or after `covered_since_ns` is
read in full, and no run whose first step started before it is read at all.
A bigger window costs more time, which the deadline bounds, and no more
memory.

**Cost.** Both passes decode every span in the window, and the second also
extracts each kept run's model-call configuration: it parses, masks and
digests each model call's messages, which is most of the work. The read
checks the deadline every 64 such extractions, so a window of very large
requests cannot overshoot it by much. The analysis is linear in runs, plus
the permutation tests in the table above: the change-point walk reads each
run once and at most two windows more per proposal, however many
excursions come and go, and it checks the deadline every 65,536 values it
reads, so a large `--radar-max-runs` cannot carry it far past the budget. A
window that cannot be read or analysed within the deadline is refused, not
cut short, so narrow `since` and `until`. Naming an `agent` does not make the read cheaper, because a run's
agent is only known after it has been read.

**Measured cost.** On an Apple M1 Max with a release build, on a laptop
shared with other work: 20,000 runs of six spans each (120,000 spans, every
model call carrying a 2 KB system prompt) took 1.3–1.6 s over three
requests; a whole-corpus `/v1/stats/duration` over the same spans took
0.21 s. With one million plain HTTP traces added to the same window, the
radar took 4.0–7.8 s (the first request reading cold segments), still
analysed exactly the 20,000 agent runs, and grew the server's resident
memory by at most 47 MB; `/v1/stats/duration` over those 1.12 million spans
took 1.4 s.

## Tenancy

A bound credential reads only its own tenant's spans, and its cohorts are
built from nothing else. Naming another tenant is
`403 {"required": "unbound"}`. An unbound caller sees every tenant's cohorts,
each labelled with its tenant, unless it passes `tenant=`. Every table a
request builds (value names and samples, step and tool names, their quotas)
is keyed by tenant as well as by value, so nothing from another tenant can
reach an answer, and a value one tenant shapes to collide with another's
digest can only confuse its own cohorts. The MCP tool runs under the same
binding.

## Limitations

- **Correlation in time, not causation.** A change point says a
  configuration changed and the outcomes moved at the same moment. When
  several things changed at once, `co_changes` lists them all, because the
  radar cannot tell which one did it.
- **The tests assume runs are independent.** A burst of retries of one task
  is not independent, and inflates significance. Sparse rate tables are
  tested exactly (see above), and every metric needs 10 observations of its
  own on each side to count, but Welch's test still assumes roughly normal
  log-scale values, which a side of 10 runs cannot check.
- **Many changes are many tests.** Holm's correction covers the tests of one
  change, not the whole timeline. Across dozens of change points, expect
  an occasional false verdict.
- **Takeovers, not mixtures.** A change point is a value taking over. A
  split that persists, or a rollout that leaves more than a fifth of runs on
  the old value for a window after the new one takes over, is not one. A
  rollout that holds a long mixed stage is found when it completes only
  about half the time. When an agent's runs carry a set of values that
  varies by branch (a router whose specialist differs per request), the
  sets form a mixture, and a change inside them is found only when the new
  set takes over.
- **Short cohorts are noisier.** Below about 200 runs the detection window
  is short, and a random split can pass as a change: about 1 cohort in 20 at
  100 runs, in simulation.
- **Volatile-token masking is a heuristic.** It can miss per-request content
  (the dimension then shows up as `volatile`). It can also hide an edit that
  only touched a long number or a date.
- **Offloaded requests are not read.** When ingest offloaded a whole message
  list to the payload store (only past `--payload-threshold-bytes`, 256 KiB
  by default), that step has no system-prompt value.
- **Runs that straddle `since`** are analysed from their spans inside the
  window. Their top-level path and first step may be cut. A run that went
  quiet for more than 15 minutes before `until` and carried on after it is
  analysed as finished, from its spans inside the window.
- **Agent identity is a name.** Renaming an agent or its service starts a
  new cohort.
- **Behaviour detection needs history.** It needs at least 60 runs, and
  testing on half of them costs power (see above).
- **Steps that start in the same nanosecond** are ordered by a hash of their
  identity, not by the tape's id order. This only affects the path tokens of
  simultaneous steps.
- **Nothing is cached.** Each request reads its window again, twice, and its
  cost grows with the spans in that window.
- **A placed run is read against the reported changes.** `downstream_of` is
  computed from the agent's ranked changes, at most 40, so it misses a
  change that was cut by that bound.
- `--radar-max-runs` applies to the server process (the HTTP route and the
  MCP tool). Library callers pass `ChangeQuery::max_runs` themselves.
