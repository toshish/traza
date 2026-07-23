# Leg 5: bundled dashboard

## Scope — allowlisted files ONLY

`src/dashboard.rs` (new: the embedded assets + route helper),
`src/bin/traza-server.rs` (route wiring only), `tests/dashboard.rs` (new),
`README.md` (one section), this doc. Anything else is out of scope.

## Design

A dependency-free trace browser served by the binary at `GET /` and
`GET /dashboard/*`:

- One self-contained HTML page (inline CSS + vanilla JS, embedded in the
  binary via `include_str!`), dark-and-light friendly.
- Views: recent spans (from `GET /v1/spans?limit=100`), a trace waterfall
  (spans of one trace laid out on a time axis via `GET /v1/traces/{id}`),
  span detail (attributes, events), and a filter bar (service, name,
  attr key/value, min duration) mapped 1:1 onto the existing query params.
- When auth is enabled the page prompts for a bearer token once and sends
  it on every fetch (stored in sessionStorage only).
- No new endpoints: the dashboard consumes the existing JSON API only.

## Acceptance (blocking)

1. `./ci.sh` green; every existing test unmodified and passing.
2. `tests/dashboard.rs`: `GET /` returns 200 with `text/html` and the
   page marker; `GET /dashboard/app.js` (if split) serves; the page
   references only same-origin API paths (no external URLs in the HTML —
   a grep oracle); with `TRAZA_TOKENS` set, `GET /` stays open (the shell
   loads) while API calls remain gated.
3. Diff-scope oracle: only allowlisted paths changed.

## Non-goals

Frameworks, build steps, npm, websockets, live tail, editing.
