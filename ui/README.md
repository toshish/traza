# Traza dashboard (ui/)

The trace browser: a React + Vite app built to the Traza design system —
the design tokens and component styles live in
[`src/styles.css`](src/styles.css). It is a single-page app that talks to a
`traza-server` JSON API.

`npm run build` emits `dist/` — one self-contained `index.html` — and
`traza-server` **serves that directory** (`--ui-dir`, default `./ui/dist`).
Nothing is compiled into the binary: building the server needs no Node
toolchain, and rebuilding the UI is picked up without restarting the server.
`dist/` is git-ignored; build it where you deploy, or host it anywhere else
that can reach the API.

## Working on it

```sh
npm ci
npm run dev     # dev server on :5173, /v1 proxied to localhost:8080
npm run build   # vite build -> dist/ (served by traza-server)
```

Run a server beside `npm run dev` for live data:
`cargo run --release --bin traza-server -- --data-dir ./data --port 8080`.

## Shape

- `src/styles.css` — design tokens (paper/dark themes, type, space, the
  measured ramp and categorical ladder, density) + base.
- `src/components/`
  - `primitives/Chrome.jsx` — Card, Chip, Eyebrow, Kbd, Figure, LoadingBar,
    empty and error states. The small surfaces every screen is built from.
  - `charts/Marks.jsx` — the drawn marks: sparklines, volume brush,
    distribution with percentile marks, time ruler and axis, share bar.
  - `nav/` — the rail, the header, the command palette.
  - `data/`, `trace/` — attribute tree, code block, message list.
- `src/views/` — one file per screen, seventeen of them.
- `src/lib/`
  - `api.js` — API client. Bearer token in sessionStorage, 401 → token prompt.
    Reads take an `AbortSignal`, identical in-flight GETs are coalesced, and
    paging uses the server's cursor.
  - `query.js` — **the query as a value.** Predicates serialize into the hash
    route, out of it, into API parameters and into curl. Sharing, saved views,
    "copy as curl" and the matching export all fall out of that one
    representation.
  - `route.js` — hash routing, `useRead` (aborts the superseded request),
    `usePoll` (stops in a background tab), `useKeys`, `useStored`.
  - `format.js`, `spans.js` — formatting, span-tree and critical-path helpers.
- Routing is hash-based (`#/traces`, `#/trace/<id>`, …), so the app works from
  any static host without server-side route rewrites.

Design rules that reviewers hold the line on: one accent (terracotta),
reserved for measured values — the `--measure-*` ramp is more of that one hue,
never a second; comparisons use the ink ladder (`--series-*`). Mono +
`tabular-nums` for every figure; flat surfaces with hairline borders; no
spinners (the loading bar), no emoji, no exclamation marks in copy.

Two rules specific to this rebuild:

- **A percentile is never reported as a mean.** Where the API gives both, the
  screen shows the percentile; where it gives a bucketed percentile, the screen
  states the error bound rather than implying exactness.
- **A number that can be clicked, is.** Every count links to the rows behind
  it; a red error count that goes nowhere is the thing this rebuild set out to
  remove.
