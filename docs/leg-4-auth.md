# Leg 4: bearer-token auth

## Scope — allowlisted files ONLY

`src/auth.rs` (new), `src/bin/traza-server.rs` (route wiring only — no
storage or protocol changes), `tests/auth.rs` (new), `README.md` (one
section), this doc. Touching `src/lib.rs`, `src/segment_v2.rs`, or any
existing test is out of scope and gate-rejectable.

## Design

- Tokens come from the `TRAZA_TOKENS` environment variable:
  `token1:rw,token2:ro` (comma-separated `token:scope`, scopes `ro`|`rw`).
  Unset variable = auth disabled (current open behavior, for dev).
- Requests carry `Authorization: Bearer <token>`.
- Scope enforcement: `ro` may GET; `rw` may GET and POST.
- Comparison must be constant-time (fold over byte equality; no early
  return on mismatch).
- Failures: missing/unknown token -> 401 `{"error":"unauthorized"}`;
  valid token, insufficient scope -> 403 `{"error":"forbidden"}`.
- No TLS (reverse-proxy territory), no token persistence, no new deps.

## Acceptance (blocking)

1. `./ci.sh` green; every existing test unmodified and passing (auth
   disabled by default keeps them green).
2. `tests/auth.rs` process-level matrix: no auth env = open; with tokens:
   no header 401, wrong token 401, ro GET 200, ro POST 403, rw POST 200 —
   for `/v1/spans`, `/v1/traces` (OTLP), and `/v1/flush`.
3. Diff-scope oracle: only the allowlisted paths changed.

## Non-goals

TLS, token rotation APIs, per-endpoint policies beyond ro/rw.
