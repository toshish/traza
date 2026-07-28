# The trace browser

Traza ships a dashboard: a React single-page app that talks to the same public
`/v1` API any client uses. Seventeen screens, grouped by the question you
arrived with — what happened, how much did it cost, how good was it, is the
server healthy.

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

## Authentication

The dashboard **shell** loads without credentials — it is static build output
carrying no data. Every `/v1` call it makes is gated exactly like any other
client.

When the server has `TRAZA_TOKENS` set, the first API call returns `401` and
the page prompts for a token. Use **Set token** in the header. The token is
held in `sessionStorage` only: it does not persist across a browser restart and
is never written to `localStorage` or a cookie. A `403` tells you the token
lacks the scope for that action — read-only tokens cannot flush or annotate.

## Navigation

Routing is hash-based (`#/spans`, `#/traces/<id>`, …), so the app works from
any static host with no server-side rewrites, and the URL of any view is
shareable.

A left rail, grouped by intent. Four top tabs could not hold seventeen
screens, and the content area is full-bleed so a waterfall gets the pixels it
needs.

| Group | Screens |
|---|---|
| **explore** | Overview `#/overview` · Traces `#/traces` · Sessions `#/sessions` · Live tail `#/tail` |
| **measure** | Analytics `#/analytics` · Latency `#/latency` · Failures `#/failures` |
| **evaluate** | Scores `#/scores` · Experiments `#/experiments` · Datasets `#/datasets` |
| **operate** | Server `#/server` · Store `#/store` · Connect `#/connect` · MCP `#/mcp` |

Detail screens hang off those: `#/trace/<id>`, `#/sessions/<id>`,
`#/conversation/traces/<id>`, `#/compare?a=…&b=…`.

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
| `esc` | Reset a waterfall zoom, or close the palette |

### Density and theme

Both are document-level and persist across reloads. Density switches every
table between comfortable (34px rows) and dense (26px); the theme toggle
switches the whole token set between paper and the inverted notebook.

## Overview

`#/overview` — the screen that answers "where should I look" before you have
decided what to look at. Five tiles (spans, errors, p95, spend, tokens) each
with a sparkline and a change against the previous period, then three "worth a
look" cards ranked by how much they moved, then the server's own summary and
recent sessions. Every number on it is a link into the screen that explains it.

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
- **Agent mode** hides framework and HTTP plumbing that carries no model call
  and no error.

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
the per-trace breakdown.

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

## Connect

`#/connect` — first run. Two environment variables, then watch the first span
arrive; the page polls and tells you the moment it does.

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
