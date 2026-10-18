# Steer: live runs, nudges and topology

The [run guard](guard.md) already follows every live agent run in memory —
its usage, its verdict — and is asked before each step whether the step may
run. Steering puts a person in that loop:

- **[Live runs](#live-runs)** — which runs are working right now, what
  step each is on, what it has spent, and its verdict, as a listing and as a
  stream.
- **[Nudges](#nudges)** — words for a running agent, or a veto of its next
  step, delivered once, through the check the agent already makes before
  each step.
- **[Topology](#topology)** — which agents a run involved, who delegated to
  whom, who handed control to whom, and where two agents bounced it back and
  forth; and a [fuse limit](#handoff-cycles-as-a-fuse-limit) on that
  ping-pong.

Nothing here calls a model or judges anything. Live runs and nudges are
guard state plus one durable log; the topology is computation over a run's
stored spans. For Claude Code, the [hook pack](../../examples/claude-code/README.md)
wires all of it in with one command.

## Live runs

```sh
curl http://127.0.0.1:8080/v1/runs/live
```

```json
{"runs":[{"agent":"triage","cost_usd":0.0,"current":{"kind":"llm","name":"gpt-4o-mini","pending":false,"started_ns":1792000004000001000},"errors":0,"last_activity_ns":1792298718444962000,"pending_nudges":0,"policy":"support","reasons":[],"run":"sess-42","service":"support-app","started_ns":1792000000000000000,"steps":8,"tenant":"","tokens":1840,"verdict":"continue"}],"truncated":false,"window_ns":900000000000}
```

A run is **live** while its last **activity** — a span arriving, or a
pre-call check — is within `--live-window-seconds` (default 900). Runs are
listed most recently *changed* first: a change is activity, a verdict that
moved, or a nudge queued or delivered. Reading a run's verdict is neither, so
a dashboard polling a run cannot keep it live.

| Field | What it is |
|---|---|
| `agent` | The agent the run is: its first step's agent name (`gen_ai.agent.name` and the other [agent keys](../llm-semantics.md#steps-and-runs)), else its first span's service. |
| `current` | The step the run is on. When a pre-call check named its next step and no span has arrived since, that step, with `pending: true` and the server's clock — the tool an agent is *about* to call. Otherwise the latest-starting recorded step, by the span's clock. |
| `started_ns` | The run's first span's start. |
| `steps`, `cost_usd`, `tokens`, `errors`, `verdict`, `reasons`, `policy` | As the [guard](guard.md#runs) counts and judges them. |
| `pending_nudges` | Nudges queued and not yet delivered or expired. |

The listing is built from the guard's live state alone — no store read — and
costs what it returns: runs are indexed by change, across tenants and per
tenant, so it walks from the newest and stops at its `limit` (default 100,
at most 1,000) or at the first change older than the window. What the view
needs beside the guard's own state — each live run's agent name, its
earliest and latest step, the step a check named — is a few hundred bytes
per live run, under the guard's `--fuse-max-runs` bound.

**What it shows is what this process has seen.** A run the server began
following after a restart lists with the usage seen since, exactly like
`GET /v1/fuses/runs`; asking for the run's verdict rebuilds it whole. A run
with no span yet holds no guard state, so it is not listed until its first
span arrives, however often it checks. Stopped runs evicted from memory
(*parked*) are not live.

### The stream

`GET /v1/runs/live/stream` is the same view as server-sent events: an
`event: snapshot` carrying the listing first (up to 256 runs), then `event: run` with one
run's row each time that run changes, and a `: tick` comment after 15
seconds of quiet. Updates are coalesced — at most one batch per 250 ms, a
run that changed several times appearing once with its latest state. When
more than 256 runs change at once, or an erasure sweeps the view, the stream
sends a fresh `snapshot` that replaces everything the client holds.
Reconnecting starts from a snapshot; there is no resume cursor.

```
event: snapshot
data: {"runs":[{"agent":"triage",…,"run":"swarm-1",…}],"truncated":false,"window_ns":900000000000}

event: run
data: {"agent":"triage",…,"current":{"kind":"tool","name":"refund_order","pending":true,"started_ns":1792297739658473000},…}
```

A stream follows the guard's change feed by reading an atomic counter every
200 ms; ingest adds one atomic store per run it touches and nothing else.
One server serves at most 64 streams at once — the 65th is refused with
`409` — and each holds one connection.

## Nudges

```sh
curl -X POST http://127.0.0.1:8080/v1/runs/sess-42/nudges \
  -d '{"message": "Hold refunds over $500 for a human.", "kind": "block", "ttl_seconds": 600}'
```

A nudge is a person's message to one run, queued until the run's next
[pre-call check](guard.md#the-ack-is-the-brake--and-it-brakes-the-next-step).
That check carries every pending nudge in a new `nudges` field —
`[{"id", "kind", "message"}]`, in the order they were queued — and marks
them delivered, durably, before it answers. Each nudge reaches **at most
one** check.

```json
{"evidence":[],"grant":null,"limits":{"max_cost_usd":2.0,"max_handoff_cycles":2},"nudges":[{"id":"nd_394a4ad29672c47e","kind":"advice","message":"Billing owns refunds; do not hand them back to triage."},{"id":"nd_e03d16ad2736feaa","kind":"block","message":"Hold refunds over $500 for a human."}],"policy":"support","reasons":["Hold refunds over $500 for a human."],"run":"sess-42","source":"rebuilt","tenant":"","tripped_at_ns":null,"tripped_by":null,"usage":{…},"verdict":"stop"}
```

- **`advice`** — words for the agent. The verdict is the guard's own.
- **`block`** — a veto. The check that delivers it answers `stop`, with the
  blocking messages as its first reasons (the guard's own follow when it
  said `stop` too). Nothing about the fuse changes: no trip, no event, no
  sticky stop. So it vetoes the one step that was asked about, and the next
  check is the guard's again. To stop a run until someone resets it, use the
  [kill switch](guard.md#stopping-and-resetting).

Only `POST /v1/fuses/check` delivers. The verdict route
(`GET /v1/fuses/runs/{run}`) and the MCP `check_run_guard` tool do not: an
agent's hook and a dashboard reading the same run must not race for its
nudges. There is no MCP tool to read or queue nudges, deliberately — an
agent hears nudges through its check, and queueing one is a person's act.

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
| Nudges kept per run | 64 | delivered and expired ones dropped, oldest first |
| Runs with nudges per tenant | 10,000 | the history of the run nudged longest ago with nothing pending is dropped; when every run has something pending, `409` |

A run need not have started: a nudge can wait for its first check. An
[erasure](http-api.md#post-v1erasures) of a run's session or trace, or of its
tenant, removes its nudges and rewrites `steer.jsonl` without them; while
such an erasure is pending, nudging the run is refused with `409`.

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
{"cycles":[{"agents":["billing","triage"],"count":1,"example":{"span_id":"a3","trace_id":"4bf92f3577b34da6a3ce929d0e0e4736"},"first_ns":1792000004000000000,"last_ns":1792000004000000000},{"agents":["triage","billing"],"count":1,"example":{"span_id":"a2","trace_id":"4bf92f3577b34da6a3ce929d0e0e4736"},"first_ns":1792000003000000000,"last_ns":1792000003000000000}],"edges":[{"count":2,"errors":0,"first_ns":1792000002000000000,"from":"support-app","last_ns":1792000004000000000,"to":"billing"},{"count":2,"errors":0,"first_ns":1792000001000000000,"from":"support-app","last_ns":1792000003000000000,"to":"triage"}],"edges_truncated":false,"handoffs":[{"count":2,"from":"triage","to":"billing"},{"count":1,"from":"billing","to":"triage"}],"nodes":[{"agent":"billing",…,"named":true,"spans":4,"steps":4,…},{"agent":"triage",…},{"agent":"support-app",…,"named":false,…}],"nodes_truncated":false,"run":"sess-42","sequence":["triage","billing","triage","billing"],"sequence_truncated":false,"spans_read":9,"tenant":"","truncated":false}
```

Why per ordered pair: a supervisor that sends work to one worker three times
in a row (`S W S W S W S`) has three `(S, W)` returns — control came back to
the supervisor three times — and two `(W, S)`. Two agents bouncing a task
between them with neither finishing it pile up both. A supervisor fanning
out to different workers (`S A S B S C S`) returns once from each, and no
worker ever gets control straight back.

Bounded: at most 50,000 spans are read (the earliest, in step order —
`truncated` says so), and the answer keeps at most 256 nodes, 1,024 edges,
256 cycle pairs and 500 turns of the sequence, the largest first, each list
saying when it was cut. Agent names are cut at 160 characters. The topology
reads the store, so it works under `--no-fuses` too.

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
test holds it to a full recount of the store after every batch. Each named
step costs about a hundred bytes of guard memory beside its span
contribution, and is bounded with it: every named step is a span the guard's
1,000,000-contribution bound already counts.

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
  turns.
- **`list_live_runs`** — the live listing; listed only when the guard is on.

## Limitations

- **Delivery is at most once, and needs a check.** A nudge is marked
  delivered when the check carrying it answers; an answer lost on the way
  (a timeout, a crashed client) loses the nudge with it, though it still lists
  as delivered. An agent that never checks never hears its nudges, and one
  that is not taking steps hears them only at its next one.
- **A veto blocks one step.** A `block` nudge stops the step that was asked
  about; the agent may try another. Use the kill switch to stop the run.
- **The live view is per process and partial after a restart.** It is built
  from memory; a run followed since some point shows what arrived since,
  and live state is never shared across replicas.
- **The stream polls.** Changes reach a stream within about 200 ms, plus the
  250 ms coalescing; a stream has no resume cursor.
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
