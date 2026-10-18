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
          "at_ns": 1792129080000000000,
          "behavior_shift": {"js_distance": 0.0, "p_value": 1.0},
          "co_changes": ["service_version"],
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
      "first_run_ns": 1792125480000000000,
      "last_run_ns": 1792132620000000000,
      "runs": 120,
      "summary": "support-bot: regression at 2026-10-16T05:38:00Z: system prompt changed (b9a28b7198c279cb → 118dd41ec67c4887), together with the service version; error rate 5.0% → 40.0% (p<0.001); success rate 95.0% → 60.0% (p<0.001).",
      "tenant": "",
      "truncated": false,
      "volatile": []
    }
  ],
  "runs_scanned": 120,
  "since_ns": 1789706291590131000,
  "spans_scanned": 360,
  "truncated": false,
  "until_ns": 1792298291590131000
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
{"agent":"support-bot","downstream_of":[{"at_ns":1792130160000000000,"dimension":"system_prompt","explanation":"error rate 5.0% → 40.0% (p<0.001); success rate 95.0% → 60.0% (p<0.001)","to":"118dd41ec67c4887","verdict":"regression"},{"at_ns":1792130160000000000,"dimension":"service_version","explanation":"error rate 5.0% → 40.0% (p<0.001); success rate 95.0% → 60.0% (p<0.001)","to":"1.5.0","verdict":"regression"}],"errored":true,"fingerprint":{"model_requested":"gpt-4o","model_returned":"gpt-4o-2024-08-06","path":"llm:gpt-4o-2024-08-06 > tool:lookup_order > llm:gpt-4o-2024-08-06","sdk_version":"opentelemetry/1.27.0","service_version":"1.5.0","system_prompt":"118dd41ec67c4887"},"outcome":"failure","run":"ticket-076","start_ns":1792131120000000000,"tenant":""}
```

`fingerprint` is the run's value for each dimension it carries, plus its
top-level `path`. `downstream_of` lists, for each dimension, the latest
change point at or before the run's start whose new value the run carries,
regressions first. A run that still carries an old value (a straggler on the
previous version) is not downstream of that change. For a `behavior` change,
any run that started after it counts as downstream. `downstream_of` is empty
when the run used values older than every change in the window. A run that is
not among the window's agent runs gets `404`. A run id used in two tenants
needs `tenant=` from an unbound caller, or the request is `400`.

## What counts as a change

Each run gets a **configuration fingerprint**, computed from its spans on
every read. A run that carries several values of one dimension (a planner
model and an executor model, say) has the set of them as its value, shown
joined with ` + `.

| Dimension | Read from (first present wins) | Shown as |
|---|---|---|
| `system_prompt` | The request's `system` and `developer` messages, with `gen_ai.system_instructions` folded in (the [step request](../llm-semantics.md#input-and-output) precedence: `gen_ai.input.messages`, indexed `gen_ai.prompt.{i}.*`, OpenInference `llm.input_messages.*`, …). When the request is a provider body (OpenInference `input.value`), its top-level `system` and its `messages` | 16 hex characters of SHA-256 over the text, volatile tokens masked |
| `tool_catalog` | `gen_ai.tool.definitions`, else indexed `llm.request.functions.{i}.*` (OpenLLMetry), else indexed `llm.tools.{i}.tool.json_schema` (OpenInference), else a provider request body's `tools` | 16 hex characters of SHA-256 over the definitions' canonical JSON, sorted, so their order does not matter |
| `model_requested` | `gen_ai.request.model`, else `llm.model`, else `llm.model_name` | the value |
| `model_returned` | `gen_ai.response.model` | the value |
| `params` | Every `gen_ai.request.*` key except the model, plus `llm.invocation_parameters` without its `model` key. These are the keys a [run diff](http-api.md#get-v1diff) calls `config` | `key=value` pairs in key order |
| `service_version` | `service.version`, on any span of the run | the value |
| `sdk_version` | `telemetry.sdk.name` and `telemetry.sdk.version`, on any span | `name/version` |
| `behavior` | The run's top-level path (see below), when no configuration change explains a shift | the most common path on each side |

The first five are read from model-call steps. A value longer than 160
characters is identified by its digest and shown cut short.

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
  request that the masking above does not catch.

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

- A value becomes **dominant** when it holds at least half of the window and
  leads the current value by `max(2, window/10)` runs. That is when a change
  point is recorded.
- The change point is placed at the first new-value run of the split that
  best separates old from new inside that window. For a clean deploy this is
  exactly the first run on the new value. For a ramped rollout it is where
  the new value took over.
- A value that **never becomes dominant** is not a change point. A 5%
  canary and a one-off fall in this group. An even A/B split usually does
  too. Random runs of one arm can briefly give it the lead, but that usually
  ends as a withdrawn excursion (next point) or a `neutral` verdict.
- A newcomer that **loses dominance again within one window** was an
  excursion, and its change point is withdrawn.
- While the first value has fewer than three runs, the dimension is still
  **settling**. A takeover then is not a change.
- A **fresh value** at the end of the window is a change point once it has
  three runs, even before it fills half a window, provided it has held
  every run since it appeared. This is how a deploy from an hour ago shows
  up, usually as `insufficient_data`. The agent's `summary` names it
  ("Since then: … (too early to judge)") when a more significant change
  heads the ranking.

### Behaviour changes

Sometimes the agent behaves differently with no configuration change at all,
because the provider changed under you. To catch this, each run gets a
**top-level path**: its first four step tokens (`kind:name`, as a
[tape](replay.md) signature has them), with consecutive repeats bucketed `x`,
`x×2`, `x×3-5` and `x×6+`, and ` > …` when more steps follow. For example:
`llm:gpt-4o > tool:search×2 > llm:gpt-4o > …`.

The distribution of paths in adjacent windows of `clamp(runs/6, 15, 100)`
runs is compared by **Jensen–Shannon distance** (base 2, from 0 for identical
distributions to 1 for disjoint ones). A peak that meets all three of these
conditions is reported as a `behavior` change:

- its distance is at or above the threshold (`behavior_threshold`, default
  0.3, which is roughly a sixth of runs moving to a new path);
- a **permutation test** calls it significant (p < 0.01, from 199
  relabellings, seeded from the agent's name so the same data gives the same
  p-value on every run);
- no configuration change point lies within a window of it.

When both sides of a configuration change have at least 10 runs, the change
also carries `behavior_shift`: the same distance and p-value between its two
sides (at most 200 runs each). This shows whether the agent took different
paths after the prompt changed.

## Deltas and verdicts

Each change compares two sides:

- **before**: runs on the old value, between the previous change point and
  this one;
- **after**: runs on the new value, between this change point and the next.

Change points of other dimensions within two runs of this one are
co-changes. They are listed in `co_changes` and do not cut the sides. Each
side is capped at the 2,000 runs nearest the change. **Runs still active**
(last span within the 15-minute session-idle window, and no declared
outcome) are left out, because their cost and steps are partial. For a
`behavior` change, the sides are all runs between the neighbouring change
points.

| Delta | Per run | Test |
|---|---|---|
| `error_rate` | at least one span with status `error` | pooled two-proportion z-test |
| `success_rate` | ended `success`, of the runs that ended at all ([outcome](../llm-semantics.md#session-outcome-and-goal--traza-extensions): declared, else the last span to end) | pooled two-proportion z-test |
| `cost_per_run` | cost in USD, metered or [priced](../configuration.md#model-pricing) | Welch's t-test on `ln(cost)` over priced runs |
| `steps_per_run` | llm, tool and agent spans | Welch's t-test on `ln(1 + steps)` |
| `duration_ns_per_run` | first start to last end | Welch's t-test on `ln(1 + milliseconds)` |
| `tool_mix` | each tool's share of tool calls | shown, not tested |

`before` and `after` are proportions (0 to 1) or arithmetic means. `ratio`
is `after / before`. Each delta also reports how many runs it counted on
each side (`n_before`, `n_after`). The mean tests run on a log scale, so a
single runaway run cannot decide a verdict. When no run on either side is
priced, the cost delta is reported empty rather than as a confident zero.
The p-values come from in-module implementations of the normal and
Student-t distributions, checked in tests against SciPy reference values.

Holm's correction holds the family-wise error rate at 5% across a change's
tests: up to five, since a test is left out when it cannot be computed (no
priced runs, or no variation at all). The verdict follows these rules, in
order:

1. `insufficient_data`: fewer than 10 finished runs on either side. The
   deltas are still reported for you to weigh.
2. `regression`: a **quality** metric (error rate or success rate) got
   significantly worse.
3. `improvement`: a quality metric got significantly better.
4. If quality held: `regression` when an **efficiency** metric (cost, steps,
   duration) got significantly worse, `improvement` when one got
   significantly better.
5. `neutral` otherwise.

`explanation` lists the metrics that decided the verdict, with any that moved
the other way ("— though: …", "— at a price: …"). `effect_size` is the largest
absolute effect among them: Cohen's h for rates and Cohen's d for log-scale
means. Changes are ranked by verdict (regressions first), then by effect size,
then newest first.

Each change also carries:

- `detail`, which depends on the dimension:
  - for a **system prompt**, the first differing stretch of the masked text
    (≤ 80 characters a side, with a little context and the character offset);
  - for a **tool catalog**, the tools added and removed, or
    `definitions_changed` when the names are the same;
  - for **sampling parameters**, the keys that differ;
  - for **behaviour**, the five paths whose share moved most.
- `example_runs`, the three runs nearest the change on each side.

Each agent also has `current` (the dominant value of each dimension over its
most recent runs) and `summary`, a one-sentence headline.

## Reading the answer

`GET /v1/changes` answers
`{"since_ns", "until_ns", "covered_since_ns"?, "spans_scanned", "runs_scanned",
"agents": [...], "run"?, "truncated"}`. Each agent is
`{"agent", "tenant", "runs", "first_run_ns", "last_run_ns", "summary",
"current", "volatile", "changes": [...], "truncated"}`. The
[HTTP reference](http-api.md#get-v1changes) has every field.

## Bounds and cost

| Bound | Value |
|---|---|
| Window | `since`/`until`. The default is the 30 days up to now |
| Runs read | `--radar-max-runs` (default 20,000): the newest agent runs. Past it, `truncated: true` and `covered_since_ns` say where the analysed window starts. Every run starting at or after that point is read in full |
| Earliest steps kept per run | 32 (enough for the top-level path) |
| Values per dimension per run | 4 (then `+ …`) |
| Distinct values per dimension | 2,048 (then `(other)`) |
| Agents | 100, the busiest (then `truncated: true`) |
| Changes per agent | 40, after ranking (then the agent's `truncated: true`) |
| Behaviour candidates tested per agent | 16, the strongest distance peaks; each costs one 199-relabelling permutation test |
| Runs let go and remembered | 1,000,000. Past it the request is refused with `400`; narrow `since` |
| Compute | One read of the window, then the analysis, all under one [`--query-deadline-ms`](../configuration.md#server-flags) budget. An analysis that runs out of budget is refused with `400` and no partial answer, and counted in `traza_query_deadline_exceeded_total` |

The run cap is exact, not approximate. When the window holds more runs than
the cap, the read lets runs go:

1. Runs with no step so far go first, so plain request traces in the same
   store cannot crowd agent runs out. Letting these go does not move the
   start of the analysed window.
2. Then the agent runs that started earliest go.

Every run that is let go is remembered, so a later span cannot bring it back
as a shorter run. If a run with no steps is let go and a step of its arrives
later, it is an agent run that is now missing spans. The analysed window
then moves past its start. As a result, every agent run that starts after
`covered_since_ns` is read in full, and no run before it is read at all.

**Measured cost.** On an Apple M1 Max with a release build: 20,000 runs of
six spans each (120,000 spans, every model call carrying a 2 KB system
prompt) were analysed in 1.0–1.4 s over three requests. Most of that time is
spent reading the spans and parsing each model call's messages. The analysis
itself is linear in runs, apart from a permutation test per behaviour
candidate. A window that cannot be read within the deadline is refused, not
cut short, so narrow `since` and `until`. Naming an `agent` does not make the
read cheaper, because a run's agent is only known after it has been read.

## Tenancy

A bound credential reads only its own tenant's spans, and its cohorts are
built from nothing else. Naming another tenant is
`403 {"required": "unbound"}`. An unbound caller sees every tenant's cohorts,
each labelled with its tenant, unless it passes `tenant=`. Value tables are
built per request from the spans the caller may read, so nothing from another
tenant can reach an answer. The MCP tool runs under the same binding.

## Limitations

- **Correlation in time, not causation.** A change point says a
  configuration changed and the outcomes moved at the same moment. When
  several things changed at once, `co_changes` lists them all, because the
  radar cannot tell which one did it.
- **The tests assume runs are independent.** A burst of retries of one task
  is not independent, and inflates significance. The normal approximation of
  the z-test is weak for very small or very lopsided samples, which is part
  of why a verdict needs 10 runs on each side.
- **Many changes are many tests.** Holm's correction covers the tests of one
  change, not the whole timeline. Across dozens of change points, expect
  an occasional false verdict.
- **Volatile-token masking is a heuristic.** It can miss per-request content
  (the dimension then shows up as `volatile`). It can also hide an edit that
  only touched a long number or a date.
- **Offloaded requests are not read.** When ingest offloaded a whole message
  list to the payload store (only past `--payload-threshold-bytes`, 256 KiB
  by default), that step has no system-prompt value.
- **Runs that straddle `since`** are analysed from their spans inside the
  window. Their top-level path and first step may be cut.
- **Agent identity is a name.** Renaming an agent or its service starts a
  new cohort.
- **Behaviour detection needs history.** It needs at least 30 runs (two
  windows of 15). The permutation test's resolution is 1/200.
- **Steps that start in the same nanosecond** are ordered by a hash of their
  identity, not by the tape's id order. This only affects the path tokens of
  simultaneous steps.
- **Nothing is cached.** Each request reads its window again, and its cost
  grows with the spans in that window.
- **A placed run is read against the reported changes.** `downstream_of` is
  computed from the agent's ranked changes, at most 40, so it misses a
  change that was cut by that bound.
- `--radar-max-runs` applies to the server process (the HTTP route and the
  MCP tool). Library callers pass `ChangeQuery::max_runs` themselves.
