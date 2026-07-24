# Traza dashboard (ui/)

The bundled trace browser: a React + Vite app built to the Traza design
system (tokens and components imported from the "Traza Design System"
project on claude.ai/design). The build compiles to **one self-contained
HTML file** that is checked in as `../src/dashboard.html` and embedded in
`traza-server` via `include_str!` — building the server itself needs no
Node toolchain, and the one-binary rule holds.

## Working on it

```sh
npm ci
npm run dev     # dev server on :5173, /v1 proxied to localhost:8080
npm run build   # vite build + scripts/embed.mjs -> ../src/dashboard.html
```

Run a real server beside `npm run dev` for live data:
`cargo run --release --bin traza-server -- --data-dir ./data --port 8080`.

After `npm run build`, rebuild the server and the new page ships at `/`.
Never edit `src/dashboard.html` by hand — it is generated.

## Shape

- `src/styles.css` — design tokens (paper/dark themes, type, space) + base.
- `src/components/` — the design-system port: primitives, data, trace,
  charts, feedback. Keep these faithful to the design project; app-specific
  composition belongs in views.
- `src/views/` — Spans, Trace, Sessions, Analytics, Store.
- `src/lib/` — API client (bearer token in sessionStorage, 401 → token
  prompt), formatting, span-tree helpers.
- Routing is hash-based (`#/spans`, `#/traces/<id>`, …) because the server
  deliberately serves only `/` and `/dashboard`.

Design rules that reviewers hold the line on: one accent (terracotta),
reserved for measured values; mono + `tabular-nums` for every figure; flat
surfaces with hairline borders; no spinners (the loading bar), no emoji,
no exclamation marks in copy.
