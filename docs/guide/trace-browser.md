# The trace browser

Traza ships a dashboard: a React single-page app that talks to the same public
`/v1` API any client uses. It shows recent spans, per-trace waterfalls, span
detail, sessions, conversations, and LLM analytics.

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

Four tabs across the top:

| Tab | Route | Shows |
|---|---|---|
| **Spans** | `#/spans` | Filtered span search — the default landing view |
| **Sessions** | `#/sessions` | One card per session, most recent activity first |
| **Analytics** | `#/analytics` | LLM token and cost rollups |
| **Store** | `#/store` | Store statistics, flush, and dataset export |

Breadcrumbs appear once you are deeper than a tab, and Back always falls
through to the parent view rather than stepping back through every span you
clicked.

## Spans

The landing view. A six-field filter form — service, name, one `attr` key and
value, minimum duration in milliseconds, and a since/until window — maps
directly onto [`GET /v1/spans`](http-api.md#get-v1spans) parameters. Applied
filters show as chips you can remove individually.

Results are a table, 100 rows a page, with **Load more** for the next page.
Clicking a row opens its trace.

With an empty store the view tells you how to point an OTel exporter at this
server, using the page's own origin.

## Trace

`#/traces/<trace_id>` — the waterfall. Spans are laid out in tree order with a
bar per span sized and positioned by its real start and duration, so nesting
and overlap are visible at a glance.

Clicking a span selects it (the URL gains `?span=…`, replacing rather than
pushing history, so Back leaves the trace instead of walking your clicks) and
opens **Span detail**: identity, timings, status, service, and the full
attribute tree. Attributes render as an expandable tree rather than a JSON
blob, and payload references open in a modal that fetches the offloaded bytes
from [`GET /v1/payloads`](http-api.md#get-v1payloadsreference).

Where a span carries prompt and completion content, a **Messages** panel
renders it. It recognizes all three shapes described in
[LLM semantics](../llm-semantics.md#prompt-and-completion-payloads): current
OTel GenAI JSON messages, the legacy indexed `gen_ai.prompt.{i}.*` attributes,
and native `llm.prompt` / `llm.completion` events.

**Annotate** attaches a score or comment to the selected span (or to the trace)
via [`POST /v1/annotations`](http-api.md#post-v1annotations). Existing
annotations appear on a timeline beside the waterfall and as chips on the spans
they judge. This needs an `rw` token when authentication is on.

If the trace belongs to a session, a link jumps to that session.

## Conversation

`#/traces/<id>/conversation` and `#/sessions/<id>/conversation` — the same
data as the waterfall, read as a transcript rather than a timeline. Every LLM
span's messages in time order, flattened into the sequence a human wants to
read.

The reason this is a separate view: consecutive turns re-send the whole
history, so replaying every span's prompt verbatim shows the same user message
a dozen times. Each turn contributes only what is new since the previous one. A
waterfall answers "what ran"; this answers "what was said".

## Sessions

`#/sessions` — one card per session, most recent activity first, backed by
[`GET /v1/sessions`](http-api.md#get-v1sessions). Each card carries the span
and trace counts, token totals, cost, error count, and activity window.

Opening a session (`#/sessions/<id>`) shows the aggregate tiles plus the
per-trace breakdown from
[`GET /v1/sessions/{id}`](http-api.md#get-v1sessionsid). From there you can
open any trace, read the whole conversation, or jump to Spans filtered to that
session.

## Analytics

`#/analytics` — LLM rollups from
[`GET /v1/stats/llm`](http-api.md#get-v1statsllm). Group by model, provider,
service, session, or day; scope to all time, the last hour, the last 24 hours,
or the last 7 days. Totals for calls, tokens, and cost sit above a table and a
chart of the selected grouping.

Cost appears only where your ingest supplied it — cost is a Traza extension,
not an OpenTelemetry GenAI attribute. See
[LLM semantics](../llm-semantics.md#recognized-attributes).

## Store

`#/store` — the operator corner of the UI. Store statistics from
[`GET /v1/stats`](http-api.md#get-v1stats) as tiles, a **Flush** button
(`POST /v1/flush`, needs `rw`), and a **Dataset export** form that builds a
[`GET /v1/export`](http-api.md#get-v1export) query from the same filter fields
as the Spans view and downloads the NDJSON.

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
