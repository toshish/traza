# The trace browser

Traza ships a dashboard: a React single-page app that talks to the same public
`/v1` API any client uses. Twenty-two screens, grouped by the question you
arrived with — what happened, may this run go on, how much did it cost, how
good was it, is the server healthy.

## Getting it running

The dashboard is a **build artifact, not part of the binary**. `traza-server`
compiles with no Node toolchain and embeds no HTML; it serves whatever built
dashboard it finds on disk.

```sh
cd ui && npm ci && npm run build   # emits ui/dist
```

Then start the server and open <http://localhost:8080/>. With no build
available the API runs exactly the same and `/` returns a `404` naming every
directory searched and the command to build it. Where the server looks, and how
to point it elsewhere, is in
[deployment](../operations/deployment.md#serving-the-dashboard).

Because it is served from disk rather than compiled in, rebuilding the UI is
picked up without restarting the server.

### Behind a path prefix

A reverse proxy that mounts a store under a path — `https://host/x/org/project/`
— serves the dashboard from that prefix, and the API with it. The page learns
the prefix from one tag in `dist/index.html`:

```html
<meta name="traza-api-base" content="/x/org/project">
```

Every request, the live tail, MCP calls and every copyable snippet (curl
commands, the OTLP endpoint on Connect, the replay base URLs) are built from
it. It is empty by default, which is right whenever `traza-server` serves the
dashboard itself. A proxy can rewrite the tag as it serves the page, or a
deployment can edit the built file. The base is read from the page, never
from the URL, so a link cannot choose where the page sends its bearer token.

## Authentication

The dashboard **shell** loads without credentials — it is static build output
carrying no data. Every `/v1` call it makes is gated exactly like any other
client.

When the server has credentials configured (`TRAZA_TOKENS`, or runtime
credentials), the first API call returns `401` and the page prompts for a
token. Use **Set token** in the header. The token is held in `sessionStorage`
only: it does not persist across a browser restart and is never written to
`localStorage` or a cookie.

**After a `401`, the page stops asking.** Every poll on the page stands down
and every request is refused in the browser until a token is set, so a
revoked or mistyped token produces one round of `401`s, not one every few
seconds from each screen. Requests already in flight when the first `401`
arrives still complete. Only a `401` for the token in use counts: one for a
request sent with an earlier token, or with none, that comes back after a new
token was set blocks nothing and opens no prompt.

**A `403` names what was missing.** The server's refusal carries `required`
(`rw`, `admin` or `unbound`), and the page says it: a read-only token on the
kill switch reads "This credential is read-only; this action needs the rw
scope", a tenant-bound token naming another tenant reads "This credential is
bound to a different tenant".

### Who the page is talking as

On load, and whenever the token changes, the page asks
[`GET /v1/whoami`](http-api.md#get-v1whoami) and keeps the answer: the
auth mode, the scope, the tenant binding, and whether the credential is an
**operator** (unbound, or an open store). Everything tenant-aware follows from
it, and nothing that depends on it runs until it has answered. If it fails
for a reason other than `401` (the server restarting, a `503` at the
connection limit, a dropped connection), the page treats the credential as
not an operator, and the rail reads `identity unknown`, while it asks again
2, 5 and then every 15 seconds until it gets an answer.

- **The rail says who you are**: `tenant acme · rw` for a bound credential,
  `all tenants · wal` for an unbound one (the second figure is the store's
  durability mode, which only an operator may read), `open store` when no
  credential exists.
- **A tenant-bound credential never calls an operator surface.** `/v1/stats`,
  `/v1/metrics*`, `/v1/verify`, `/v1/flush`, `/v1/checkpoint`,
  `/v1/backups/*`, `/v1/admin/*`, `/v1/ledger` and `/v1/ledger/verify` would
  answer it `403`, and they are not asked: the API client refuses them in the
  browser for a non-operator, so even a screen that forgot to check cannot
  send one. The rail's ingest pulse and the header's count come from the
  tenant's own row of [`GET /v1/usage`](http-api.md#get-v1usage) instead —
  spans its tenant has sent — and Overview shows that row against its limits
  where an operator sees the server.
- **Operator screens are not offered.** Server, Store and Credentials leave
  the rail and the command palette for a non-operator. A pasted link to one
  explains why instead of asking the server.

### Handing a token over in the URL

A control plane that mints a credential for someone can send them straight
in with it, in the URL **fragment** — the part a browser never sends to a
server:

| Fragment | Opens |
|---|---|
| `#token=<t>` | The default screen, with `<t>` as the token |
| `#/<route>?…&token=<t>&…` | That route, with `token` removed and every other parameter kept |

`<t>` is percent-decoded, and only that: a `+` stays a `+` in both forms, and
the other route parameters are kept exactly as written. The token is taken
before the first render, and again on every fragment navigation: a handoff
link opened in a tab that already shows the dashboard changes only the
fragment, so the page does not reload, and the token is taken then and
treated as a token change. Either way it is stored exactly as **Set token**
stores it and removed from the address bar with `history.replaceState`. That
rewrites the current history entry, including the one a fragment navigation
has just made, so neither it nor a bookmark keeps the token. A `token` in the
query string (before the `#`, which servers and proxies log) is ignored. The
page that linked here may still hold the full URL in its own history; send
handoff links over a channel you would send the token over.

## Navigation

Routing is hash-based (`#/traces`, `#/trace/<id>`, …), so the app works from
any static host with no server-side rewrites, and the URL of any view is
shareable.

A left rail, grouped by intent. Four top tabs could not hold the screens, and
the content area is full-bleed so a waterfall gets the pixels it needs. The
rail and the header's controls are real buttons, reachable by keyboard.

| Group | Screens |
|---|---|
| **explore** | Overview `#/overview` · Traces `#/traces` · Sessions `#/sessions` · Live tail `#/tail` |
| **runs** | Guard `#/guard` · Diff `#/diff` |
| **measure** | Analytics `#/analytics` · Latency `#/latency` · Failures `#/failures` |
| **evaluate** | Scores `#/scores` · Experiments `#/experiments` · Datasets `#/datasets` |
| **operate** | Server `#/server`\* · Store `#/store`\* · Ledger `#/ledger` · Credentials `#/credentials`\* · Connect `#/connect` · MCP `#/mcp` |

\* operator only: shown to an unbound credential or an open store.

Detail screens hang off those: `#/trace/<id>`, `#/sessions/<id>`,
`#/run/<run>`, `#/conversation/traces/<id>`, `#/compare?a=…&b=…`,
`#/diff?a=…&b=…`.

**A run's tenant travels in its route.** Links from Guard, a session or a
trace to a run, from a run to its diff, and from a diff step to its tape carry
`?tenant=<t>`, and `?tenant=` (empty) for the default tenant. The empty value
matters: on the tape, diff and replay routes, naming no tenant is the operator
view across every tenant, so a default-tenant `weather-7` opened without it
would read every tenant's `weather-7` interleaved. The Sessions list links each
session with its tenant for the same reason. A run opened without one (from
the command palette, or a typed link) is read the way the server reads it:
a tenant-bound credential sees its own tenant, and an operator sees a note
that the tape spans every tenant, no guard verdict, and a field to name the
tenant (empty: the default).

**A query lives in the URL.** The Traces screen serializes its predicates,
window, sort and limit into the hash, so the search that found the bug is a
link you can send. Editing a predicate replaces the history entry rather than
pushing one, so Back leaves the screen instead of walking backwards through
every keystroke.

### Keyboard

| Key | Does |
|---|---|
| `⌘K` / `Ctrl-K` | Command palette — jump to a screen, or paste a trace or session id to open it directly |
| `j` / `k` | Move the row cursor (Traces, Scores, the waterfall) |
| `↵` | Open what the cursor is on |
| `/` | Focus the predicate builder |
| `e` | Export the current query |
| `a` | Toggle agent mode in a trace |
| `esc` | Close a dialog, the palette or the narrow-width menu; reset a waterfall zoom |

Dialogs keep `Tab` inside themselves and give focus back to whatever opened
them when they close.

### Density and theme

Both are document-level and persist across reloads. Density switches every
table between comfortable (34px rows) and dense (26px); the theme toggle
switches the whole token set between paper and the inverted notebook. Until
the toggle is used the theme follows the operating system's light or dark
preference, live; a choice, once made, is applied before the first paint, so
a dark reload does not flash paper.

### Narrow screens

Below 900px the rail leaves the layout and opens as a drawer from **Menu** in
the header; choosing a screen closes it. Below 700px stat tiles drop to two
columns, side-by-side panels stack, and wide tables (the waterfall, the tape,
the aligned diff, the ledger) scroll sideways inside their cards rather than
widening the page. The trace detail stacks its waterfall above its inspector
below 1150px, where the pair no longer fits side by side.

## Overview

`#/overview` — the screen that answers "where should I look" before you have
decided what to look at. Five tiles (spans, errors, p95, spend, tokens) each
with a sparkline and a change against the previous period, then three "worth a
look" cards ranked by how much they moved, then the server's own summary and
recent sessions. Every number on it is a link into the screen that explains it.
A tenant-bound credential sees its tenant's usage — this month's spans and
bytes against its limits, the quota state, and spans refused — where an
operator sees the server's summary.

The comparison is one request: the window is fetched as
[`GET /v1/stats/series`](http-api.md#get-v1statsseries) and split in half, so
"since yesterday" costs one scan rather than two.

## Traces

`#/traces` — text search over a predicate builder.

**Content search takes the full width and the first position**, because it is
the one filter you can use without already knowing the schema. It maps to
[`GET /v1/spans?content=`](http-api.md#content-search-content): word matching,
not substring and not a phrase, so `refund` finds "Refund the order" and not
"refunds", and several words are ANDed in any order. `/` focuses it.

Below it, a **predicate list** rather than a fixed form. Each row is field /
operator / value, and the operator picks the parameter family: `=` is
`attr.KEY`, `≠` is `not_attr.KEY`, `≥` and `≤` are `min_attr` / `max_attr`. Two
predicates on one key send it twice, which the old single-pair form could not
express at all. `status` maps to the span's own status field, not an attribute.

Above the table: a **volume chart you can drag** to set the window, and the
range presets. Below it, the **query cost** — how long the query took, how
many segments were read, and what fraction the time filter pruned — read
straight off the response envelope rather than asserted.

Columns sort via `sort=`; paging uses the response cursor, so **Load more**
costs one page instead of re-fetching everything already on screen.

**Copy as curl** reproduces the exact query in a terminal. **Export NDJSON**
and **Make a dataset** carry it to the Store and Datasets screens, so the
export is the search rather than a retype.

## Trace

`#/trace/<id>` — the waterfall, as a reading instrument.

- A **time ruler** with real tick labels, and a **minimap** of the whole trace
  with the zoom window drawn on it.
- **Drag on the ruler to zoom**; `esc` resets.
- **Carets collapse a subtree**, which is what makes a 200-span agent trace
  readable.
- The **critical path** — the chain that determines the trace's duration — is
  drawn in the accent; everything off it is muted. Shortening an off-path span
  cannot make the trace faster.
- **Self time** is a separate column and a darker segment at the head of each
  bar: the part of a span that is its own work rather than a child's.
  Overlapping children are unioned, not summed, so concurrent tool calls do
  not produce negative self time.
- **Agent mode** hides framework and HTTP plumbing: it keeps the spans whose
  [step kind](../llm-semantics.md#steps-and-runs) is a model call, a tool call
  or an agent invocation (OTel GenAI, OpenLLMetry and OpenInference spans
  alike), and any span that failed.
- **Run** opens the trace's run on its tape: the session its root span
  carries, else `trace:<id>`, in the root span's tenant.

The time ruler fits its tick count to its width, so its labels never overlap,
however narrow the column.

Selecting a span puts `?span=…` in the URL (replacing, not pushing) and opens
its detail: timings, model and usage, the attribute tree, events, offloaded
payloads, and its scores. A failed span offers the failure group it belongs to
and a comparison against a clean run.

## Conversation

`#/conversation/traces/<id>` and `#/conversation/sessions/<id>` — the same
data read as a transcript. A **turn rail** down the left lists every turn with
its cost or latency, so the expensive turn is findable without reading the
whole exchange.

Consecutive turns re-send the whole history, so replaying every span's prompt
verbatim would show the same user message a dozen times. Each turn contributes
only what is new since the previous one. A waterfall answers "what ran"; this
answers "what was said".

## Sessions

`#/sessions` — a sortable table with the time window the API always had.
Sort by recency, cost, errors, length, or **cost per turn** — the efficiency
figure a team is actually managing. Opening one shows the aggregate tiles and
the per-trace breakdown, and **Run** opens it as a run: a session id is a run
id. A session is one tenant's (`(tenant, session_id)` is its identity), so
the list opens each in its own tenant and **Run** keeps it there.

## Guard

`#/guard` — the runs the [run guard](guard.md) holds, from
[`GET /v1/fuses/runs`](http-api.md#get-v1fusesruns), refreshed every five
seconds. The server lists runs most recently active first, and a stopped run
takes no more steps, so past a page of busy runs it would drop off the end.
The screen therefore asks the server three things: the stopped runs
(`state=stop`), the warning runs (`state=warn`), and the most recently active
runs, up to 200 each. The default view shows stopped runs first, then
warnings, then the rest, each run once and each group most recently active
first. Chips filter by verdict, with a count on each: `stopped` and `warning`
are the server's own filters; `continuing` has no server filter, so it covers
the continuing runs among the 200 most recently active. A count reads `200+`
when there were more than the screen shows, and a line under the list says
so. An operator can narrow to one tenant. The listing is the guard's memory:
a run it is not following appears once something asks about it.

Each run shows its verdict, its tenant (to an operator), the policy it is held
to, when and how a stop tripped, any grant, and the reasons in the server's
words. Below them, **one bar per limit in force** — cost, tokens, steps, LLM
and tool calls, duration, repeats, error streak — drawn against the limit with
a mark at 80%, where the guard starts to warn; a limit the policy does not set
is unbounded and gets no bar. The most repeated step's fingerprint and the
span that tripped a stop are linked as evidence.

To a credential with `rw`:

- **Kill switch** stops a run until it is reset, durably, with an optional
  reason ([trip](http-api.md#post-v1fusesrunsruntrip)).
- **Approve to continue** resets a stopped run, optionally with a **grant**:
  higher limits, as absolute values, for that run only
  ([reset](http-api.md#post-v1fusesrunsrunreset)). A reset does not refill
  cumulative usage; a run still past a cost, token, step or duration limit is
  stopped again at once, and the page says so and points at the grant.
- **Policies** can be created, edited and deleted: name, the service whose
  runs it holds (none for `default`), the eight limits, and `warn` or `stop`.

The **global default** — the server's `--fuse-*` flags, which every run with
no matching or `default` policy gets — is shown above the runs. A
read-only credential sees all of it and none of the buttons. On a server
started with `--no-fuses` the screen says so instead.

## Run

`#/run/<run>` — a run as a **tape**, from
[`GET /v1/tapes/{run}`](http-api.md#get-v1tapesrun): its model calls, tool
calls and agent invocations in start order. A run id is a session id, or
`trace:<trace_id>` for a trace with none; sessions and traces link here by
exactly that rule.

Tiles total the steps, duration, tokens and cost (the cost counts only steps
that reported one, and says how many did), errors, and the spans read. Each
step shows its kind, what it called (model, tool or agent), when it ran on a
shared time bar, its duration, input and output tokens, cost and status.
Opening a step shows its fingerprint, its request and its response — model
messages as a transcript, tool arguments and results as JSON — with their
digests, and a link to the span in its trace. An offloaded value shows its
preview and loads on request; **Inline payloads** re-reads the tape with
`resolve=true`, within the server's `--tape-max-bytes`, and says when some
stayed references. The screen reads at most 2,000 steps, and says when a tape
is cut. A diff linking to a step opens the tape with that step open
(`?step=`). A step past the first 2,000 is read in whole pages of 2,000, up
to the server's 20,000, so the step is on the tape; a step the run does not
have is named as missing, with the number of steps the tape does have.

**Replay this run** creates a [replay](replay.md) (`strict` or `loose`) and
shows what an agent needs to run against it: `OPENAI_BASE_URL` /
`OPENAI_API_KEY` and `ANTHROPIC_BASE_URL` / `ANTHROPIC_API_KEY` lines built
from the page's API base, and six lines of Python that send step 0's recorded
messages through the OpenAI SDK — which a strict replay matches — and print
the recorded answer. The key is written as `$TRAZA_TOKEN` (any non-empty
value on an open server); the page never prints the token itself. **What has
it served?** reads the replay's status: served, remaining, and the first
divergence.

**Compare with…** takes another run id, with recent sessions offered as
suggestions, and opens the diff.

## Diff

`#/diff?a=<run>&b=<run>` — two runs aligned step by step, from
[`GET /v1/diff`](http-api.md#get-v1diff). Without both runs it asks for them.

A totals table sets steps, tokens, cost, duration, errors and outcome of
`a` beside `b`, with `b − a`. The **first divergence** sits above the
alignment: its kind (`input`, `decision`, `config`, `structure`), what that
kind means, the server's explanation, and links to the step on each side.
Below, every aligned pair with its op (`same`, `changed`, `insert`,
`delete`), the step index on each side, both fingerprints with the part that
differs drawn in the accent, and why; the first divergence's row is
highlighted. Every step index opens that step on its run's tape, in the
tenant the diff was read in. **Swap** reverses the comparison. A diff with no
tenant named reads, for an operator, every tenant's runs of those ids, and the
page says so above the totals, with a field to name the tenant.

## Live tail

`#/tail` — spans as they land, with a pause bar and the arrival rate beside
them. Polling is incremental: each tick asks only for spans newer than the
last one seen, so the cost of watching is proportional to what arrived rather
than to what is on screen. Pausing buffers and tells you how many are waiting.

## Analytics

`#/analytics` — a **measure switcher** over any grouping. Cost, tokens, calls,
latency, errors, cost per call, and cost per 1k tokens, grouped by model,
provider, service, session or day, from
[`GET /v1/stats/llm`](http-api.md#get-v1statsllm).

Cost appears only where your ingest supplied it — cost is a Traza extension,
not an OpenTelemetry GenAI attribute. See
[LLM semantics](../llm-semantics.md#recognized-attributes).

## Latency

`#/latency` — the distribution and the traces behind its tail, from
[`GET /v1/stats/duration`](http-api.md#get-v1statsduration). Percentile marks
follow the system's one drawn convention: a solid ink hairline for the median,
dashed for the tail. Below it, p95 over time — each bar a bucket's 95th
percentile, never a mean — and the ten slowest spans, ranked by the server
across the whole match set rather than within a page.

## Failures

`#/failures` — errors grouped by `(service, name, status)` from
[`GET /v1/stats/failures`](http-api.md#get-v1statsfailures), with share, p50,
p95, first and last seen. Every row opens its most recent example. "3 errors"
used to be a red number that did nothing; the distance from noticing a failure
to reading one is now a click.

## Scores

`#/scores` — annotations across traces, which needed
[`GET /v1/annotations`](http-api.md#get-v1annotations) to stop requiring a
`trace_id`. Numeric scores get a distribution, everything else a tally, each
split human vs eval. Below that a **review queue**: `j`/`k` to move, `↵` to
open the trace being judged.

## Experiments

`#/experiments` — A/B two cohorts on spans, percentiles and errors. A cohort is
just a predicate, so anything you can filter by you can compare.

## Datasets

`#/datasets` — a saved search promoted to an eval set, with the export command
it corresponds to. Datasets live in the browser: a dataset **is** a query, and
the query is already reproducible from the curl command, so persisting it
server-side would add a stateful surface holding nothing the store does not.

## Server

Operator only, like Store and Credentials.

`#/server` — what this process has actually done. Uptime, spans admitted,
requests answered, and **request latency split by route class** so search,
lookup, stats and ingest are separable. Ingest and query rates are differenced
client-side from the counters. Time-range pruning is stated as a proportion:
how much of the store your windows have been eliminating.

Percentiles carry their accuracy bound on screen — they are bucket upper
bounds, at most 6.25% high and never low. See
[monitoring](../operations/monitoring.md).

## Store

`#/store` — the durability statement in words, the segment map, **Flush**
(`POST /v1/flush`, needs `rw`), and a dataset export that reuses the query you
arrived with rather than making you retype it.

## Ledger

`#/ledger` — the [ledger](ledger.md): the hash chain over everything the
store committed, and the evidence pack.

Every credential sees the **head** ([`GET /v1/ledger/head`](http-api.md#get-v1ledgerhead)):
the tip's hash, sequence number, store id and when it was appended — what you
would hand a notary to anchor.

**Download evidence pack** fetches [`GET /v1/evidence`](http-api.md#get-v1evidence)
for a window (the last day, week, month, or all time) and saves it as JSON
exactly as the server sent it: the pack's digest covers its canonical JSON
and its timestamps exceed 2^53, so re-serializing it in the browser could
change the bytes the digest is over. The page then shows the digest, the
spans counted, the entries and the verification verdict. A bound credential
always gets its own tenant's pack; an operator chooses the whole store or one
tenant.

An operator also sees:

- **Verify** — [`GET /v1/ledger/verify`](http-api.md#get-v1ledgerverify):
  intact or broken, the entry count, whether the live generation's manifest
  matches the ledger and its files verify, and every problem by name. It
  re-reads every manifested byte, so it costs what `GET /v1/verify` costs.
- **Anchors** — the external countersignatures among the loaded entries.
- **Entries** — the chain in order, two hundred at a time, each with its
  kind-specific detail (generation and manifest digest, erasure subject kind
  and digest, anchor authority and target).

The page shows what the server reports and recomputes no head itself; the
ledger guide has a short script that does.

## Credentials

`#/credentials` — runtime credentials, for an unbound `admin`
([`/v1/admin/credentials`](http-api.md#post-v1admincredentials)).

**Mint** takes a scope (`ro`, `rw`, `admin`), an optional tenant binding,
a label and an expiry, and shows the new token **once**, with a copy button:
the store keeps only its SHA-256. **Use in this tab** switches the page to
it. The list shows every credential — environment ones first — with its id,
prefix, scope, tenant, label, source, when it was created, when it expires,
when it was last used (kept in server memory only) and whether it is live,
expired or revoked. **Revoke** works on runtime credentials; an environment
credential is revoked by restarting without it.

On an open store the screen says what minting does: the first credential must
be an unbound `admin`, and from that request on every client must
authenticate. The page adopts that first token at once — otherwise its own
next poll would answer `401` over the one-time display — and says so.

## Connect

`#/connect` — first run. Two environment variables, then watch the first span
arrive; the page polls and tells you the moment it does. An operator watches
the store's record count; a tenant-bound credential watches its own tenant's
sent spans (`GET /v1/usage`, falling back to one span of `GET /v1/spans`), so
"waiting for the first span" ends when the first span it can see arrives.
The snippets use the page's API base and turn gzip on (Traza decompresses it
itself); on a server that requires a token they include the bearer header,
with the token left as `$TRAZA_TOKEN`.

## MCP

`#/mcp` — connecting an agent to *read* this store, which is the other
direction from Connect's "send spans in".

Everything on the page is asked of the running server rather than written into
the build: whether the endpoint is serving, which protocol revision it
negotiated, and the exact tools, resources, templates and prompts your token
would be offered. A screen that listed what the build believes the surface to
be would be wrong the moment the server was started without
`--mcp-annotations`, and wrong in the direction that costs somebody an
afternoon.

With the endpoint off, the page says so and gives the flags. With it on, it
generates both client configurations — the HTTP form and the stdio-bridge form
— against the origin you are actually on, so they are correct behind any host,
port or reverse proxy. If a bearer token is set in this browser session it is
included in those snippets, and the page says so.

Full reference: [the MCP guide](mcp.md).

## Developing on the dashboard

```sh
cd ui
npm ci
npm run dev     # Vite on :5173, /v1 proxied to localhost:8080
npm test        # vitest
npm run build   # -> dist/
```

Run a server alongside `npm run dev` for live data. `npm test` and
`npm run build` are both part of [`./ci.sh`](../../CONTRIBUTING.md) — a
dashboard that does not build or whose tests fail does not merge. See
[`ui/README.md`](../../ui/README.md) for the component layout and the design
rules reviewers hold the line on.
