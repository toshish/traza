# Run fuses

A **fuse** holds an agent run to limits — cost, tokens, steps, LLM calls,
tool calls, duration, repeated steps, consecutive failures — and gives the run
a **verdict**:

| Verdict | Means |
|---|---|
| `continue` | Nothing is near a limit. |
| `warn` | The run is at 80% or more of some limit (or reached one under a `warn` policy). It may go on. |
| `stop` | The run reached a limit under a `stop` policy, or an operator stopped it. **Sticky**: it stays `stop` until the run is reset. |

The store computes verdicts from the spans it ingests. Nothing here calls a
model or runs a judge: a fuse is arithmetic over what was recorded.

## The ack is the brake — and it brakes the next step

Spans arrive when operations **end**. By the time an LLM call's span reaches
Traza, the call has been made and paid for; by the time a tool span arrives,
the tool has run. So the verdict an ingest acknowledgement carries can only
stop what comes **after** the batch it acknowledges:

```
step N runs ─▶ its span ends ─▶ exporter sends it ─▶ ack says "stop" ─▶ step N+1 must not start
```

And exporters batch: the OpenTelemetry SDK's batch processor sends every
5 seconds by default, so an agent can take several more steps between
crossing a limit and hearing about it. Two consequences follow, and the API
is shaped around both:

- **The pre-call gate is [`POST /v1/fuses/check`](http-api.md#post-v1fusescheck).**
  Asked *before* a step — optionally with the step's estimated cost, tokens
  and tool — it answers whether the step may run. A step that **would go
  past** a stop limit answers `stop`. This is the only surface that can
  refuse a step before it happens, and it is what an agent framework or a
  hook (see [the Claude Code example](../../examples/guard/README.md)) should
  call.
- **The post-ack verdict is a backstop.** Every ingest response names the
  runs in its batch whose verdict is not `continue`, so a client that does
  nothing but export still learns its run was stopped — at least one batch
  late.

The two agree on where a limit is. Recorded usage *at* a limit has reached
it: a run holding `max_steps: 50` whose fiftieth step is recorded is stopped,
so the fifty-first should not start. A hypothetical next step reaches a limit
only by going *past* it: a run at 49 of 50 may take its fiftieth. So the two
never disagree about step 51 — the check refuses it, and the acknowledgement
of step 50 says stop — though a client relying on acknowledgements alone may
already have started it.

## Quick start

Give a service a policy, then ask about a run. Every response below is real
output from a server started with `--fuse-max-steps 50`.

```sh
curl -X POST http://127.0.0.1:8080/v1/fuses/policies -d '{
  "name": "support", "match": {"service": "support-agent"},
  "limits": {"max_cost_usd": 2.0, "max_steps": 40, "max_repeats": 4, "max_error_streak": 3},
  "action": "stop"}'
```

```json
{"action":"stop","created_at_ns":1792062221773128000,"id":"fp_58ed88d43fc762ba","limits":{"max_cost_usd":2.0,"max_error_streak":3,"max_repeats":4,"max_steps":40},"match":{"service":"support-agent"},"name":"support","tenant":""}
```

An agent of that service calls `search_orders` with the same arguments four
times. The batch carrying the fourth call is acknowledged with the verdict,
in the body and in a header:

```
HTTP/1.1 200 OK
Traza-Fuse: stop; run=sess-7f3a

{"accepted":1,"durability":"wal","fuses":[{"reasons":["max_repeats 4 reached: tool:search_orders#465a6bab9a1c447d repeated 4 times"],"run":"sess-7f3a","tenant":"","verdict":"stop"}]}
```

Before its next step, another run of the same service asks the gate, with an
estimate:

```sh
curl -X POST http://127.0.0.1:8080/v1/fuses/check \
  -d '{"run": "sess-9c21", "next": {"est_cost_usd": 0.40}}'
```

```json
{"evidence":[{"span_id":"b1","trace_id":"4bf92f3577b34da6a3ce929d0e0e4736"}],"grant":null,"limits":{"max_cost_usd":2.0,"max_error_streak":3,"max_repeats":4,"max_steps":40},"policy":"support","reasons":["max_cost_usd 2 would be exceeded by the next step: $1.7 spent + $0.4 estimated"],"run":"sess-9c21","source":"live","tenant":"","tripped_at_ns":null,"tripped_by":null,"usage":{"cost_usd":1.7,"duration_ns":900000000,"error_streak":0,"errors":0,"llm_calls":1,"steps":1,"tokens":1920,"tool_calls":0,"top_repeat":null},"verdict":"stop"}
```

The check changed nothing: `usage` is what the run has recorded, and the run
itself is still `warn` (85% of its budget). Every route is in the
[HTTP reference](http-api.md#run-fuses).

## Runs

A run is one unit of agent work within a tenant. Its id is the span's
session id (`session.id`, then `gen_ai.conversation.id`, then the Traceloop
association properties) or, for a span with none, `trace:<trace_id>`. So a
conversation spread over many traces is one run, and an uninstrumented trace
is a run of its own. The full rules are in
[LLM semantics](../llm-semantics.md#steps-and-runs).

Membership is decided **per span**. A trace whose root carries `session.id`
but whose children do not puts the children in `trace:<id>`, not in the
session — instrument every span of a run with its session id, or the run
the fuse sees is smaller than the one you meant.

What a run has used:

| Usage | Counted from |
|---|---|
| `steps` | Spans whose [step kind](../llm-semantics.md#steps-and-runs) is LLM, tool or agent. Retrievals, embeddings and wrapper spans are context, not steps. |
| `llm_calls`, `tool_calls` | Steps of those kinds. |
| `tokens` | Every span's reported total, else prompt plus completion — the same sum the session analytics report. A framework that reports usage on both a model call and the agent span around it is counted twice here, exactly as it is there. |
| `cost_usd` | Every span's priced cost: a metered `llm.cost_usd`, else one derived from [`--pricing`](../configuration.md#model-pricing). |
| `errors` | Spans with status `error`, of any kind. |
| `error_streak` | Consecutive failed **steps** at the end of the run. A successful step ends it; a non-step span neither extends nor ends it. |
| `duration_ns` | First span start to last span end, by the spans' own clocks. An idle gap counts once the next span arrives. |
| `top_repeat` | The step fingerprint seen most often, its count and an example span. |

A **fingerprint** is a step's kind and name plus a hash of its input —
`tool:search_orders#465a6bab9a1c447d` — so two steps share one exactly when
they made the same call with the same input. That is what a loop looks like.
The definition is in [LLM semantics](../llm-semantics.md#steps-and-runs).

A span that is ingested again is an upsert here, as it is in the store: a
client retrying a batch the store already acknowledged changes no count —
not even the error streak — and a span re-sent with different content
replaces its earlier contribution. One re-sent with only a later end time
extends the run's `duration_ns` (a re-sent span that ends *earlier* leaves
it where it was: the bounds only widen, which errs toward the limit). When
two requests write the same span concurrently, the version the store kept is
the one counted, whichever request's acknowledgement happens to be observed
last — including while the run is being rebuilt from the store. And when one
batch carries two versions of a span, only the later — the one the store
keeps — is counted, at the place its own start time gives it among the
batch's steps.

## Policies

A policy is a name, which runs it is for, a set of limits, and an action:

| Limit | Reached when |
|---|---|
| `max_cost_usd` | `cost_usd >= limit` |
| `max_tokens` | `tokens >= limit` |
| `max_steps` | `steps >= limit` |
| `max_llm_calls` | `llm_calls >= limit` |
| `max_tool_calls` | `tool_calls >= limit` |
| `max_duration_seconds` | `duration_ns >= limit × 10⁹` |
| `max_repeats` | `top_repeat.count >= limit` |
| `max_error_streak` | `error_streak >= limit` |

Every limit is optional and an absent one is unbounded. A present one must be
positive, and `max_repeats` at least 2 — one occurrence is not a repeat.
`action` is `stop` or `warn` (default `warn`, matching `--fuse-action`).

**Which policy a run gets.** Exactly one, resolved at every evaluation, so an
edit applies to live runs at once:

1. the run's tenant's policy whose `match.service` is the service of the
   run's first span (several: the most recently created). First by **start
   time**, not arrival: a root agent span ends, and so arrives, after its
   children, and the policy must not depend on which arrived first — a
   rebuild, which reads spans in start order, picks the same one. The
   run's earliest span, counting a batch's own, is found before any span of
   the batch is judged, so a root sent behind its children in the same
   batch sets the policy they are judged under. A root arriving in a
   *later* batch cannot lift a stop its children already latched under
   their own policy (see [limitations](#limitations));
2. else the tenant's policy named `default`;
3. else the **global** policy from the server's
   [`--fuse-*` flags](../configuration.md#server-flags), reported as
   `"policy": "global"`.

A non-`default` policy must name a `match.service`, and `default` must not —
otherwise one would never be chosen and the other would be ambiguous. The
name `global` is reserved. A tenant keeps at most 256 policies (creating
another is `409`), and a `match.service` is at most 256 bytes: a run's
policy is resolved among its tenant's policies on every acknowledged batch,
so one tenant's policy count must not reach into every tenant's ingest.

**Limits do not inherit.** A tenant policy that sets only `max_cost_usd`
holds its runs to that alone; the global policy's repeat limit does not
reappear underneath it. Set every limit you want on the policy you mean.

**Out of the box** the global policy warns on 12 repeats and nothing else:
the one limit that is safe without knowing the workload, and a warning,
because a default that stopped someone's agent would be a decision nobody
made.

## Stopping and resetting

A `stop` from a policy latches at the span that reaches the limit, wherever
that span falls in a batch: three failed steps and then a successful one,
sent together, stop a run held to `max_error_streak: 3` exactly as they
would sent one at a time, or replayed from the store after a restart — the
success ends the streak, not the stop. A batch's spans are judged in the
order a replay reads them — by start time, then trace and span id — not in
the order they were sent (across batches, see
[limitations](#limitations)). And it is sticky. Later spans of the
run, a raised limit, a deleted policy, eviction from memory — none of them
lift it (but see
[what is durable](#what-is-durable-and-what-is-rebuilt) for the bound on
stopped runs held, and for restarts). Two things do:

- **[Reset](http-api.md#post-v1fusesrunsrunreset)** clears the stop, the
  kill switch, the error streak and the repeat counts. It does **not** refill
  cumulative usage: a budget does not grow because someone pressed reset. A
  run reset while still over a cumulative limit stops again immediately, and
  the reset's answer says so.
- **A grant** — `{"grant": {"max_cost_usd": 5}}` on a reset — raises that
  run's limits to the absolute values named, over whatever its policy says.
  Grants accumulate per limit, later ones winning, and survive restarts.

The **[kill switch](http-api.md#post-v1fusesrunsruntrip)** stops any run at
once, whatever its usage and policy, until it is reset. It is durable, and
a killed run stays listed as stopped across restarts.

Durable controls are bounded per tenant: at most 10,000 runs with a kill
switch, a reset or a grant. Past that, a trip or reset of another run drops
the controls of the run reset longest ago — its grant and its reset stop
applying, which can only make it stricter — and when all 10,000 are active
kill switches, the trip or reset is refused with `409`. A kill switch is
never dropped to make room.

## What is durable, and what is rebuilt

Live usage is an in-memory cache over the store, folded from every
acknowledged batch on the ingest path. It is bounded, with no exceptions: at
most `--fuse-max-runs` live runs (default 100,000, shared by every tenant),
at most 1,000,000 span contributions across them (each a 40-byte record under
an 8-byte key, before hash-table overhead), least recently active evicted
first, and any run idle for six hours evicted. A run is followed from its
**first step**: a trace of HTTP and database spans costs the observer a read
of each span and holds no state. A run id longer than 1,024 bytes is not
followed at all, and asking about one is `400`.

A run evicted while **stopped** — by a policy or by hand — is **parked**:
its stop (reasons, time, evidence) and its usage as last seen stay in memory,
its per-span state does not. A later span of the run is answered `stop`, a
rebuild keeps the stop whatever the policy now says, and the run stays in
the stopped listing. Up to `--fuse-max-runs` runs are parked; past that the
oldest are dropped, and a policy stop dropped that way is recomputed by the
next rebuild under the policy then in force — which, if the policy was raised
or deleted since, no longer stops it. A dropped kill switch loses only its
place in the listing: the switch itself is durable.

The first time a verdict is asked for a run, the run is **rebuilt** from
its stored spans and the answer says `"source": "rebuilt"`; after that it is
answered from memory (`"live"`) and kept current by ingest. The process
cannot know whether a run had spans before it began following it — the run
may predate a restart, or have been evicted — so it never assumes it did
not. Asking about a run no span carries — a fresh run, or a made-up id —
answers `continue` and holds nothing.

**Part of a history answers only `stop`.** What part of a run's history
used is a lower bound on what the run used: enough to establish that it
must stop, never that it may go on. Two things leave the guard with part:

- A rebuild reads and extracts the run's spans under the same budget as any
  query, `--query-deadline-ms`. If it runs out, what memory holds of the run
  — live state followed since some point, a parked stop, a kill switch — is
  all there is. If that stops the run (a kill switch, a stop latched live or
  parked, or a checked next step that would go past a limit on what is
  known), the answer is `stop`, marked `live`; otherwise the question is
  refused with the deadline's `400`, and the rebuild is retried next time.
- A rebuild reads at most a run's earliest 100,000 spans. If the run has
  more, and those already stop it, the answer is `stop` (exactly: a replay
  of the whole run stops at the same span). If they do not, the question is
  refused with `400` — and every later one too, without reading the run
  again, until the run is evicted, since its history only grows. Such a run
  answers `stop` once something stops it — what arrives live is added to
  what was read, and a kill switch always works — but never `continue` or
  `warn`.

Either way a refusal is an error, not a `200`: a hook configured to fail
closed (the Claude Code example's `TRAZA_FUSE_FAIL_CLOSED=1`) blocks the
step, and one that fails open lets it through — the operator's choice,
rather than a `continue` that would have let the step through either way.

`fuses.jsonl` in the data directory holds what spans cannot reconstruct:
policies, kill switches, resets and grants, one fsynced line each, replayed
at startup (a torn final line from a crash is healed; damage anywhere else
refuses startup rather than silently dropping a later kill switch). A change
whose append fails part-way — a full disk — is refused and its fragment cut
back out of the file, so the next change cannot be written onto it; if even
that cut fails, every further change is refused with `503` until a restart
heals the torn tail. Most of
its lines are superseded sooner or later — a policy replaced, a kill switch
reset — so once it holds at least twice the records that reproduce the state
(and at least 1,024 lines) it is rewritten to just those, at startup or on the
next change; an erasure rewrites it too, without the erased subject. Both
rewrites are atomic, by rename. It lives
**outside** the generation layout, beside the store rather than in it: a
backup does not carry it, and a restore neither replaces nor resurrects it.
Fuse settings are operator configuration about the running service, and a
restore that reinstated last month's limits — or lifted a kill switch pulled
since — would be the more surprising outcome.

What a restart loses, and what it does not:

| | After a crash restart |
|---|---|
| Policies, kill switches, resets, grants | Kept (`fuses.jsonl`). |
| A run's usage | Rebuilt from the store on first query. |
| A policy stop | Recomputed by the rebuild under the policy in force then; `tripped_at_ns` becomes the end time of the span that crossed the limit, since the original server-clock moment was in memory. |
| The repeat counts and error streak after a reset | Recomputed from spans that **ended** after the reset, comparing span clocks with the server's reset time. |
| The event feed | Starts empty; a cursor from before the restart reads as a gap. |
| "This warn was already announced" | Lost: a rebuilt run that is still warning is announced again on its next transition. |

## Watching

- **The [event feed](http-api.md#get-v1fusesevents)** records every
  transition as it happens — a run entering `warn`, a policy `stop`, a
  `manual_stop`, a `reset` — in a ring of 4,096, read by cursor with an
  optional long-poll of up to 30 seconds. An alert dispatcher holding an
  unbound credential polls it for every tenant. A run's warn is announced
  when it enters `warn`, and once more if a `warn` policy's limit is then
  reached — the moment a `stop` policy would have stopped it — and not again
  until the run returns to `continue` or is reset.
- **[`GET /v1/fuses/runs`](http-api.md#get-v1fusesruns)** lists the runs
  the guard holds — live and parked — by verdict, most recently active
  first, from memory alone: a partly followed run lists with what this
  process has seen, a parked one with its usage as last seen live.
- **`/v1/metrics`** reports `traza_fuse_trips_total` (policy and manual
  stops), `traza_fuse_warnings_total` and the `traza_fuse_live_runs` gauge
  (live runs only; parked ones are not counted).
- **MCP**: [`check_run_guard`](mcp.md) lets an agent read its own run's
  verdict. It cannot reset or trip anything — the MCP surface has no
  actuators.

## Tenancy

Policies, runs, kill switches and events are all per tenant. A
tenant-bound credential reads and writes only its own: its policies are
created under its tenant, naming another tenant anywhere is `403`, and
another tenant's policy id is a `404`. The same run id in two tenants is two
runs under two policies. An unbound credential naming no tenant addresses the
default tenant on the single-run routes and every tenant on the listings.

`POST /v1/fuses/check` needs only the `ro` scope, though it is a `POST`: an
agent holding a read-only token must be able to ask whether it may proceed.
Every other `POST` here needs `rw`.

## Limitations

Stated plainly, so none of them is discovered in production:

- **A post-ack stop is late by construction** — at least one step, more with
  exporter batching. Only the pre-call check refuses a step before it runs,
  and only if the agent asks.
- **A step is never interrupted.** A single LLM call or tool run that runs
  away on its own is not stopped mid-flight; the fuse acts between steps.
- **The check sees recorded usage only.** Steps in flight — started, not yet
  exported — are invisible to it; parallel tool calls can all pass the gate
  before any of them is recorded.
- **A run with no spans yet has no service**, so a check before its first
  span resolves the tenant's `default` (or the global) policy, not a
  service-matched one.
- **Across batches, arrival order counts.** Within a batch, spans are
  judged in step order under the policy of the earliest span known, as a
  replay judges them; spans in later batches can only be judged as they
  arrive. Children that reach a stop limit of their own service's policy in
  a batch before their root's therefore stay stopped — sticky like any stop
  — although a rebuild after a restart, reading the root first, judges them
  under the root's policy. Likewise a failed step arriving after steps that
  started later extends the error streak where it arrived, not where it
  started. Where a run's services have different policies, keep the child
  services' limits no stricter than the root's.
- **After a restart or an eviction, ingest verdicts for a continuing run see
  only what arrived since**, until something asks for the run's verdict and
  it is rebuilt. The pre-call check is such a question, so an agent that
  checks before each step is judged on its whole history from its first
  step after the restart. A stop survives eviction (parked), but not a
  restart: a rebuild after one recomputes it under the policy then in force,
  and does not re-announce it on the event feed.
- **A run of more than 100,000 spans gets no verdict short of `stop`.** A
  rebuild reads its earliest 100,000 only, which cannot clear it (see
  [part of a history](#what-is-durable-and-what-is-rebuilt)). A run first
  asked about while still shorter — as the run of an agent that checks
  before every step is — is rebuilt whole and then followed live past that
  length, in full, until it is evicted or the server restarts.
- **Tenants share the live bound.** One tenant with many active runs can
  evict another's; those are rebuilt when asked about, and their stops are
  parked, but their ingest verdicts see only new spans until then.
- **Clocks.** Duration uses the spans' own timestamps. A rebuild compares
  span end times with the server's reset time, so a client clock far off the
  server's can put a span on the wrong side of a reset.
- **Fingerprints depend on how input was stored.** An input large enough to
  be [offloaded](ingest.md) hashes by its content address, an inline one by
  its canonical JSON, so the same input stored both ways is two
  fingerprints. Under one `--payload-threshold-bytes` the same input is
  always stored the same way.
- **Erasure removes what the guard holds about the subject, not what the
  guard concluded from it.** The guard keeps no span text, but it does keep
  identifiers: an [erasure](http-api.md#post-v1erasures) drops every live run
  a purged span contributed to (rebuilt, if asked about again, from what the
  store still holds) and every parked run it names or whose evidence names a
  purged span, removes an erased session's or trace's events and its kill
  switch, reset and grant, and a tenant erasure takes the tenant's runs,
  events and policies wholesale; for a tenant, session or trace subject
  `fuses.jsonl` is rewritten from what remains. Work in flight cannot undo
  it: a rebuild that read the store before the erasure discards what it read
  and reads again, and an ingest acknowledged just before the erasure began
  — whose observer can run after the guard was swept — has its observer skip
  the spans the pending erasure covers. The one window left is such an
  observer stalled until after the erasure has settled, which would follow
  the run again from the erased spans until it is next rebuilt or evicted. A
  `traza_fuse_*` counter, and an alert a dispatcher already sent from the
  event feed, are not taken back. A kill-switch reason belongs to its run, so
  it goes with an erased session; a span erasure that leaves the session
  standing leaves the reason too — keep personal data out of reasons.
- **One process.** Live state is per server; there is no shared state across
  replicas.

## Disabling

`--no-fuses` turns the observer off entirely: ingest does no fuse work,
`fuses.jsonl` is not read or written, responses carry no `fuses` field or
header, every `/v1/fuses/*` route answers
`409 {"error": "run fuses are disabled on this store (traza-server --no-fuses)"}`,
and MCP does not list `check_run_guard`.
