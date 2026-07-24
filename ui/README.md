# Traza dashboard (ui/)

The trace browser: a standalone React + Vite app built to the Traza design
system (tokens and components imported from the "Traza Design System"
project on claude.ai/design). It is a single-page app that talks to a
`traza-server` JSON API; it is **not** bundled into the server binary — run
or deploy it separately.

## Working on it

```sh
npm ci
npm run dev     # dev server on :5173, /v1 proxied to localhost:8080
npm run build   # vite build -> dist/ (a self-contained static SPA)
```

Run a server beside `npm run dev` for live data:
`cargo run --release --bin traza-server -- --data-dir ./data --port 8080`.

`npm run build` emits `dist/` — a self-contained static bundle you can host
anywhere (any static file server, a CDN, or behind the same reverse proxy as
the API). Point it at your server's origin; the client attaches the bearer
token and calls `/v1/*`.

## Shape

- `src/styles.css` — design tokens (paper/dark themes, type, space) + base.
- `src/components/` — the design-system port: primitives, data, trace,
  charts, feedback. Keep these faithful to the design project; app-specific
  composition belongs in views.
- `src/views/` — Spans, Trace, Sessions, Analytics, Store.
- `src/lib/` — API client (bearer token in sessionStorage, 401 → token
  prompt), formatting, span-tree helpers.
- Routing is hash-based (`#/spans`, `#/traces/<id>`, …), so the app works from
  any static host without server-side route rewrites.

Design rules that reviewers hold the line on: one accent (terracotta),
reserved for measured values; mono + `tabular-nums` for every figure; flat
surfaces with hairline borders; no spinners (the loading bar), no emoji,
no exclamation marks in copy.
