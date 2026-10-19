# Steer: live runs, nudges and topology

The [run guard](guard.md) already follows every live agent run in memory —
its usage, its verdict — and is asked before each step whether the step may
run. Steering puts a person in that loop:

- **[Live runs](#live-runs)** — which runs are working right now, what
  step each is on, what it has spent, and its verdict, as a listing and as a
  stream.
- **[Nudges](#nudges)** — words for a running agent, or a veto of its next
  step, delivered once, through the pre-call check the agent already makes
  before each step.
- **[Topology](#topology)** — which agents a run involved, who delegated to
  whom, who handed control to whom, and where two agents bounced it back and
  forth; and a [fuse limit](#handoff-cycles-as-a-fuse-limit) on that
  ping-pong.

Nothing here calls a model or judges anything. Live runs and nudges are
guard state plus one durable log; the topology is computation over a run's
stored spans. For Claude Code, the [hook pack](../../examples/claude-code/README.md)
wires all of it in with one command.

## See it run

[`examples/mission-control/run.sh`](../../examples/mission-control/README.md)
runs simulated support agents against a real server, each asking the guard
before every step. It checks the claims this guide makes — foresight on the
live listing, the stream's snapshot and rows, a run held by its history, a
nudge heard at the next check after an approval with lower odds, the kill
switch, a ping-pong run stopped at three handoff cycles, and a fork — then
keeps the agents running for the dashboard's **Live** screen until Ctrl-C.

## Live runs

```sh
curl http://127.0.0.1:8080/v1/runs/live
```

```json
{"runs":[{"agent":"triage","cost_usd":0.0,"current":{"kind":"llm","name":"gpt-6-luna","pending":false,"started_ns":1792000004000001000},"errors":0,"last_activity_ns":1792298718444962000,"pending_nudges":0,"policy":"support","reasons":[],"run":"sess-42","service":"support-app","started_ns":1792000000000000000,"steps":8,"tenant":"","tokens":1840,"verdict":"continue"}],"truncated":false,"window_ns":900000000000}
```

A run is **live** while its last **activity** — a span arriving, or a
pre-call check (a `POST /v1/fuses/check` naming the `next` step) — is within
`--live-window-seconds` (default 900). Runs are listed most recently
*changed* first: a change is activity, a verdict that moved, or a nudge
queued or delivered. Reading a run's verdict — a check naming no `next`
step included — is neither, so a dashboard polling a run cannot keep it
live.

| Field | What it is |
|---|---|
| `agent` | The agent the run is: its first step's agent name (`gen_ai.agent.name` and the other [agent keys](../llm-semantics.md#steps-and-runs)), else its first span's service. |
| `current` | The step the run is on. When a pre-call check named its next step and no span has arrived since, that step, with `pending: true` and the server's clock — the tool an agent is *about* to call. Otherwise the latest-starting recorded step, by the span's clock. |
| `started_ns` | The run's first span's start. |
| `steps`, `cost_usd`, `tokens`, `errors`, `verdict`, `reasons`, `policy` | As the [guard](guard.md#runs) counts and judges them. |
| `pending_nudges` | Nudges queued and not yet delivered or expired. |
| `foresight` | How earlier runs of its agent that took the same path ended — a verdict, the success rate with its 95% interval, and how many runs that is — read from memory; see [foresight on live runs](#foresight-on-live-runs). |
| `foresight_deferred` | `true` when the row lacks `foresight` only because the one-time read that would give it foresight was not made by this request (see [below](#foresight-on-live-runs)); absent otherwise. |

The listing is built from the guard's live state alone — no store read,
except the one-time read that gives a run its foresight (below).
Runs are indexed by change, across tenants and per tenant; a listing walks
from the newest change, stops at its `limit` (default 100, at most 1,000) or
at the first change older than the window, and skips on the way runs that
changed within the window without activity in it (a verdict that moved, a
nudge) — so it reads at most every run the guard holds, never more. What the
view needs beside the guard's own state — each live run's agent name, its
earliest and latest step, the step a check named — is a few hundred bytes
per live run, under the guard's `--fuse-max-runs` bound.

What it costs ingest: for each run a batch touches, the run is re-stamped —
one removal and one insertion in each of the two change indexes and an
atomic store, under the guard's state lock the fold already holds — and each
step span's agent and label are read beside the fingerprint the guard
already extracts (copied only when longer than 160 characters, to cut), the
agent's whole name digested once to tell it from others.

**What it shows is what this process has seen.** A run the server began
following after a restart lists with the usage seen since, exactly like
`GET /v1/fuses/runs`; asking for the run's verdict rebuilds it whole. A run
with no span yet holds no guard state, so it is not listed until its first
span arrives, however often it checks. Stopped runs evicted from memory
(*parked*) are not live.

### Foresight on live runs

Each live run carries its [foresight](foresight.md#foresight-for-one-run):

```json
"foresight": {"base_rate": 0.5444, "ci95": [0.0435, 0.2136],
              "map_built_at_ns": 1792312576819863000, "p_success": 0.1,
              "support": 50, "verdict": "likely_failure"}
```

— this run's path so far was taken by 50 earlier runs of its agent, and 5 of
them succeeded, against 54% for the agent overall. It is counted from
history, never predicted by a model. The guard keeps every live run's
behaviour path — its earliest 192 steps, 48 bytes each, inside the guard's
span budget — and the tenant's [Fate Map](foresight.md#the-fate-map) is the
cached one the predictive fuses read, so the verdict costs a walk of the path
down the map: no store read. Verdicts are judged at the predictive fuses'
default support of 30 runs; with fewer, or no map cached yet, the verdict is
`insufficient_history`.

Two things put a run's foresight off for a moment. A listing that finds no
map cached asks the background builder for one, so the first look at a
tenant shows no verdicts and the next ones, a build later, do. And a run the
guard has followed only since it came into memory — never asked about — may
be a tail of a longer run, so it is read whole from the store first, as a
verdict query reads it: at most 8 such runs per listing, newest first, once
each while they stay in memory. Those reads share the listing's one
`--query-deadline-ms` budget — a stream update has one of its own — so a
listing spends one budget on them, not one per read: once it is spent no
read starts, and a read it cut short ends the reading. A run whose read
failed is not read again for 60 seconds (at most 1,024 runs are remembered
so; an erasure forgets them), so polling does not repeat a read that cannot
finish. Every row that needed a read this request did not make says so with
`"foresight_deferred": true`, and lists without `foresight` until a later
request reads it. Runs that check before every step (the hook pack, the SDK
guards) are already whole. `--fate-cache-seconds 0` turns all of this off:
no paths are kept, and no row has `foresight`.

### The stream

`GET /v1/runs/live/stream` is the same view as server-sent events: an
`event: snapshot` carrying the listing first (up to 256 runs), then `event: run` with one
run's row each time that run changes, and a `: tick` comment after 15
seconds of quiet. When a run whose row the client holds leaves the view, the
stream says so with `event: removed` — `{"run", "tenant", "reason"}`, the
reason `evicted` (the guard let go of it to stay inside `--fuse-max-runs`,
its span budget, or after six idle hours; a stopped run is parked) or
`expired` (its last activity fell out of `--live-window-seconds`, sent within
about 200 ms of it doing so, with no other change needed) — so a client that
applies every event holds exactly the rows the listing has. Removals come
before the update's `run` events: a run evicted and back again within one
update is removed, then sent anew. Updates are coalesced — at most one batch
per 250 ms, a run that changed several times appearing once with its latest
state. When more than 256 runs change at once, an erasure sweeps the view,
the client would hold more than 4,096 rows, or more than 1,024 runs were
evicted since the stream last looked, the stream sends a fresh `snapshot`
that replaces everything the client holds. Reconnecting starts from a
snapshot; there is no resume cursor.

```
event: snapshot
data: {"runs":[{"agent":"triage",…,"run":"swarm-1",…}],"truncated":false,"window_ns":900000000000}

event: run
data: {"agent":"triage",…,"current":{"kind":"tool","name":"refund_order","pending":true,"started_ns":1792297739658473000},…}

event: removed
data: {"reason":"expired","run":"swarm-0","tenant":""}
```

To tell its client what left, a stream remembers the rows it sent and has
not removed — about 64 bytes each beside the run ids the guard already
holds, at most 4,096 a stream — and the guard remembers the last 1,024 runs
it evicted, for the streams still behind them. Both are memory only.

A stream follows the guard's change feed by reading an atomic counter every
200 ms, taking no lock until something changed. One server serves at most 64
streams at once, and one scope — a tenant, or the every-tenant view of an
unbound credential — at most 8 of them, so one tenant's credential cannot
take every slot; past either, the stream is refused with `409`. Each holds
one connection.

## Nudges

```sh
curl -X POST http://127.0.0.1:8080/v1/runs/sess-42/nudges \
  -d '{"message": "Hold refunds over $500 for a human.", "kind": "block", "ttl_seconds": 600}'
```

A nudge is a message to one run, queued until the run's next
[pre-call check](guard.md#the-ack-is-the-brake--and-it-brakes-the-next-step)
— a `POST /v1/fuses/check` that names the step it is about to take
(`"next": {...}`, `{}` for an LLM call; the hook pack and both SDKs always
send it). That check carries every pending nudge in a new `nudges` field —
`[{"id", "kind", "message"}]`, in the order they were queued — and marks
them delivered, durably, before it answers. Each nudge is delivered **at
most once**.

```json
{"evidence":[],"grant":null,"limits":{"max_cost_usd":2.0,"max_handoff_cycles":2},"nudges":[{"id":"nd_394a4ad29672c47e","kind":"advice","message":"Billing owns refunds; do not hand them back to triage."},{"id":"nd_e03d16ad2736feaa","kind":"block","message":"Hold refunds over $500 for a human."}],"policy":"support","reasons":["Hold refunds over $500 for a human."],"run":"sess-42","source":"rebuilt","tenant":"","tripped_at_ns":null,"tripped_by":null,"usage":{…},"verdict":"stop"}
```

- **`advice`** — words for the agent. The verdict is the guard's own.
- **`block`** — a veto. The check that delivers it answers `stop`, with the
  blocking messages as its first reasons (the guard's own follow when it
  said `stop` too). So does every other pre-call check of the run in the
  **two seconds** after (`BLOCK_HOLD_MS`), carrying the same block nudge
  again (same `id`; it is still delivered once): an agent that issues a
  batch of tool calls checks them together, or one right after another as
  each refused call is skipped, and a veto must refuse the batch, not
  whichever call asked first. Nothing about the fuse changes: no trip, no
  event, no sticky stop, so after the hold the guard judges the run as
  usual. To stop a run until someone resets it, use the
  [kill switch](guard.md#stopping-and-resetting).

Only a pre-call check delivers. A check naming no `next` step is a verdict
read: it answers `"nudges": []` and leaves every nudge pending, as the
verdict route (`GET /v1/fuses/runs/{run}`) and the MCP `check_run_guard`
tool do — so a monitoring script or a dashboard probing a run cannot take
the agent's nudges. Any caller that sends `next` for the run does consume
them, `ro` tokens included (the agent's own token is usually `ro`): give the
check's credential only to the agent. There is no MCP tool to read or queue
nudges, deliberately — an agent hears nudges through its check, and
queueing one is an operator's act.

**Who can queue one.** Creating a nudge needs `rw` — the same scope a
telemetry collector holds. So a nudge is operator text, not proof that a
person wrote it: the hook pack relays nudges as *queued in Traza*, and the
SDKs hand them to the caller (`Verdict.nudges` and `on_nudges` in Python,
`verdict.nudges` and `onNudges` in TypeScript) to frame as it sees fit.
Keep `rw` credentials away from anyone you would not let instruct your
agents.

`GET /v1/runs/{run}/nudges` lists a run's nudges, each with `status`:
`pending`, `delivered` (with `delivered_at_ns`) or `expired`.

### Durable, bounded, erasable

Nudges are written to `steer.jsonl` in the data directory — one fsynced line
per nudge and per delivery, through the same log machinery as
[`fuses.jsonl`](guard.md#what-is-durable-and-what-is-rebuilt): a torn final
line is healed at startup, damage anywhere else refuses startup, a failed
append is cut back out, and the file is rewritten to the surviving state once
half its lines are superseded. A delivery that cannot be written fails the
check (`503`) and leaves every nudge pending. Like `fuses.jsonl` it lives
outside the generation layout: backups do not carry it.

| Bound | Value | Past it |
|---|---|---|
| Message | 1 to 4,096 bytes | `400` |
| `ttl_seconds` | 1 to 604,800 (7 days); default 3,600 | `400`; an expired nudge is never delivered |
| Pending nudges per run | 32 | `409` |
| Blocks one delivery's hold vetoes with | 32 | the newest kept: those just delivered, then the latest still holding |
| Nudges kept per run | 64 | delivered and expired ones dropped, oldest first |
| What one tenant's nudges cost | 4 MiB | its delivered and expired nudges are dropped, oldest first; when only pending ones (and holding blocks) are left, `409` |
| What every tenant's nudges cost | 32 MiB | anyone's delivered and expired nudges are dropped, oldest first; when only pending ones (and holding blocks) are left, `409` |

A nudge costs its message, id, run id and tenant plus 512 bytes for its
place in memory and its indexes — so a tenant holds a few hundred nudges at
the longest message, a few thousand short ones. Delivered and expired
nudges are history, kept for `GET /v1/runs/{run}/nudges` and dropped first;
pending ones are never dropped to make room, and neither is a delivered
`block` whose two-second hold is still in force — it is the veto the run's
checks are being answered with, and dropping it would let the next call of
its batch through. A run's 32 pending nudges and 32 holding blocks together
never exceed the 64 it keeps, so the per-run bound always holds; a budget
that only holding blocks keep full refuses with `409` for at most those two
seconds. The file holds at most about twice what memory does before it is
rewritten.

**No check waits on another run's nudges.** The nudges in memory and the
log on disk have separate locks: queueing, delivering, forgetting and
rewriting hold the log's across their write and fsync, and take the memory
lock only for in-memory work (a rewrite, only to copy out what it will
write). A pre-call check with nothing pending — almost every check — takes
the memory lock for one lookup and answers; it never waits on a write, an
fsync or a rewrite.

A run need not have started: a nudge can wait for its first check. An
[erasure](http-api.md#post-v1erasures) of a run's session or trace, or of its
tenant, removes its nudges and rewrites `steer.jsonl` without them; while
such an erasure is pending, nudging the run is refused with `409`. That
refusal is decided under the nudge log's lock, which the erasure's own
removal takes too, so a nudge queued while an erasure starts is either
refused or removed by it — never left behind.

Messages are operator text and are delivered to the agent as written.
Keep personal data out of them as you would out of a kill-switch reason.

## Topology

```sh
curl http://127.0.0.1:8080/v1/runs/sess-42/topology
```

The agent graph of one run, computed from its stored spans under the query
deadline:

- **Nodes** are agents. A span's agent is the agent it names; a span that
  names none belongs to its parent's agent when the parent ran in the same
  service, and to its own service otherwise (a process boundary is an actor
  of its own). `named: false` marks a service standing in for an agent.
- **Edges** are parent → child span relations whose spans belong to
  different agents — delegations — with their count, failed children, and
  first and last child start.
- **Handoffs**, the **sequence** and **cycles** come from the run's
  *handoff sequence*: its steps that name an agent, in step order, with
  consecutive steps of one agent merged into one **turn**. A step naming no
  agent does not move control; it is work inside whichever agent holds it.
  A **cycle** is control returning to an agent straight from the agent it
  handed off to — turns `A, B, A` — counted per ordered pair.

```json
{"cycles":[{"agents":["billing","triage"],"count":1,"example":{"span_id":"a3","trace_id":"4bf92f3577b34da6a3ce929d0e0e4736"},"first_ns":1792000004000000000,"last_ns":1792000004000000000},{"agents":["triage","billing"],"count":1,"example":{"span_id":"a2","trace_id":"4bf92f3577b34da6a3ce929d0e0e4736"},"first_ns":1792000003000000000,"last_ns":1792000003000000000}],"cycles_truncated":false,"edges":[{"count":2,"errors":0,"first_ns":1792000002000000000,"from":"support-app","last_ns":1792000004000000000,"to":"billing"},{"count":2,"errors":0,"first_ns":1792000001000000000,"from":"support-app","last_ns":1792000003000000000,"to":"triage"}],"edges_truncated":false,"handoffs":[{"count":2,"from":"triage","to":"billing"},{"count":1,"from":"billing","to":"triage"}],"handoffs_truncated":false,"nodes":[{"agent":"billing",…,"named":true,"spans":4,"steps":4,…},{"agent":"triage",…},{"agent":"support-app",…,"named":false,…}],"nodes_truncated":false,"run":"sess-42","sequence":["triage","billing","triage","billing"],"sequence_truncated":false,"spans_read":9,"tenant":"","truncated":false}
```

Why per ordered pair: a supervisor that sends work to one worker three times
in a row (`S W S W S W S`) has three `(S, W)` returns — control came back to
the supervisor three times — and two `(W, S)`. Two agents bouncing a task
between them with neither finishing it pile up both. A supervisor fanning
out to different workers (`S A S B S C S`) returns once from each, and no
worker ever gets control straight back.

Bounded: at most 50,000 spans are read (the earliest, in step order —
`truncated` says so), and the answer keeps at most 256 nodes, 1,024 edges,
1,024 handoff pairs, 256 cycle pairs and 500 turns of the sequence — the
lists largest first, each with its own flag (`nodes_truncated`,
`edges_truncated`, `handoffs_truncated`, `cycles_truncated`,
`sequence_truncated`). Agents and services are told apart by their whole
names: two agents whose names share their first 160 characters are two
nodes, take turns, and make cycles, and an unnamed span in a service whose
name merely starts like its parent's is its own actor. A name longer than
160 characters *renders* as its first 160, `…#`, and 16 hex digits of a
digest of the whole name (`<first 160 characters>…#3f9c0a1b2c3d4e5f`), so two cut names
still render apart; the guard's stop reasons render them the same way.
Every pass
over the spans is linear or a sort, and each checks `--query-deadline-ms`
as it goes: past the deadline the request is refused with `400`, not
answered late. The topology reads the store, so it works under
`--no-fuses` too.

## Handoff cycles as a fuse limit

A [fuse policy](guard.md#policies) may set `max_handoff_cycles`: the run is
held to the largest cycle count among its agent pairs, `count >= limit`
reaching it, exactly as the topology counts — but **since the run's last
reset**, the way repeats are. `--fuse-max-handoff-cycles N` sets it on the
global policy.

```sh
curl -X POST http://127.0.0.1:8080/v1/fuses/policies -d '{
  "name": "support", "match": {"service": "support-app"},
  "limits": {"max_handoff_cycles": 2, "max_cost_usd": 2.0}, "action": "stop"}'
```

The acknowledgement of the step that made the second return to triage:

```json
{"accepted":1,"durability":"wal","fuses":[{"reasons":["max_handoff_cycles 2 reached: control went triage → billing → triage 2 times"],"run":"sess-42","tenant":"","verdict":"stop"}]}
```

The verdict's `usage` gains `top_handoff_cycle` —
`{"agents": ["triage", "billing"], "count": 2}`, the agent control returned
to first — whenever a cycle exists, and omits it otherwise. A run whose steps
name no agent holds no handoff state at all and never reaches the limit.

The guard keeps the sequence incrementally, exact under steps arriving in
any order and re-sent spans that move or rename a step: a change can only
alter the turns beside it, so each is recounted over a window of at most
nine turns, in O(log n). A rebuild replays the same fold, and a randomized
test holds it to a full recount of the store after every batch.

What it costs: about a hundred bytes of guard memory per named step beside
its span contribution — every named step is a span the guard's
1,000,000-contribution bound already counts — plus the rendered name of
each distinct agent a held step names, up to 160 characters and the digest
of a cut one, at most 256 names a run. A name is dropped when the last step
naming it is, so a run never holds more names than named steps; a span
re-sent under ever new agent names holds one. Past 256 distinct agents, the
rest render as `agent#<hex>`. Agents are told apart by a 64-bit digest of
the whole name under keys drawn at random when the server starts, so nobody
writing telemetry can choose two names the guard confuses; when two pairs
tie for the most cycles, which one a stop names may differ after a restart,
the count never.

## Tenancy

Live runs, nudges and topologies are per tenant, under the guard's rules: a
bound credential reads and writes only its own tenant, and naming another is
`403 {"error":"this credential is bound to a different tenant","required":"unbound"}`.
The same run id in two tenants is two runs with two sets of nudges. An
unbound credential naming no tenant addresses the default tenant on the
single-run routes and every tenant on the live listing and stream.

Queueing a nudge needs `rw`. Everything else here needs only `ro` — and
`POST /v1/fuses/check`, which delivers nudges, stays `ro`: the agent's
read-only token is what receives them.

## MCP

Two read tools, both rendering telemetry inside the untrusted-content
frame (see [MCP](mcp.md)):

- **`run_topology`** — a run's agents, delegations, ping-pong pairs and
  turns. When a list was cut, the headline says so (`more than 256
  agents`, and which lists show only their largest entries). The headline
  is the server's own words and counts; the run id and tenant, which
  whoever wrote the telemetry chose, are the frame's first row.
- **`list_live_runs`** — the live listing; listed only when the guard is on.

## Limitations

- **Delivery is at most once, and needs a check.** A nudge is marked
  delivered when the check carrying it answers; an answer lost on the way
  (a timeout, a crashed client) loses the nudge with it, though it still lists
  as delivered. An agent that never checks never hears its nudges, and one
  that is not taking steps hears them only at its next one.
- **A veto blocks a two-second window, not a turn.** A `block` refuses every
  pre-call check of the run for two seconds after it is delivered: the
  calls of a batch checked together or back to back. A later call of the
  same batch that waits behind a long-running one gets past it, and an
  agent that tries again within the window meets the same veto. Traza sees
  no model turns, only checks. Use the kill switch to stop the run.
- **A nudge's author is a credential, not a person.** Any `rw` credential
  can queue one, and its text reaches the agent as written.
- **The live view is per process and partial after a restart.** It is built
  from memory; a run followed since some point shows what arrived since,
  and live state is never shared across replicas.
- **The stream polls.** Changes and removals reach a stream within about
  200 ms, plus the 250 ms coalescing; a stream has no resume cursor.
- **Handoff cycles count named agents only.** A run whose agents are told
  apart only by service name gets service nodes and delegation edges in its
  topology but no turns, no cycles, and nothing for `max_handoff_cycles` to
  count. Name agents with `gen_ai.agent.name`.
- **Across batches, arrival order counts** — as it does for the
  [error streak](guard.md#limitations). The guard places a late-arriving
  named step where it started, so its counts always equal a recount of what
  it holds, but it judges the limit as steps land: a step that arrives after
  later-starting steps can latch a stop on a cycle that a step still in
  flight would have broken. With steps arriving in start order the live
  latch and a replay's agree exactly.
- **The guard counts since the last reset; the topology counts the whole
  run.** After a reset, the two differ by design.
