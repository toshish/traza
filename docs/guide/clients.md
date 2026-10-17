# Client libraries

Two thin clients for the routes an agent harness talks to — the run fuses,
replays, diffs, recall, and the ledger — live in this repository:

| | Python | TypeScript |
|---|---|---|
| Package | `traza` ([`sdk/python`](../../sdk/python/README.md)) | `@trazadb/sdk` ([`sdk/typescript`](../../sdk/typescript/README.md)) |
| Runtime | Python 3.9+ | Node 20+, Deno, Bun, edge runtimes, same-origin browser pages |
| Transport | `urllib` (standard library) | the global `fetch` |
| Runtime dependencies | none | none |
| Types | dataclasses, `py.typed` | `.d.ts`, wire field names |

**They are not instrumentation.** OpenTelemetry — or OpenLLMetry or
OpenInference on top of it — records spans, and an OTLP exporter sends them
([ingest](ingest.md)). Neither client sends spans: each asks the server about
what was recorded and acts on the answer. The one place they touch
instrumentation is `otel_env` / `otelEnv`, which writes the exporter's
environment variables.

Both are versioned with the engine (0.27.0) and tested against the
`traza-server` binary built from the same tree.

## What they cover

| Python | TypeScript | Route |
|---|---|---|
| `whoami()` | `whoami()` | [`GET /v1/whoami`](http-api.md#get-v1whoami) |
| `usage()` | `usage()` | [`GET /v1/usage`](http-api.md#get-v1usage) |
| `check(run, est_cost_usd, est_tokens, tool)` | `check(run, {estCostUsd, estTokens, tool})` | [`POST /v1/fuses/check`](http-api.md#post-v1fusescheck) |
| `guard(run, …)` — context manager and decorator | `guard(run, …)`, `guarded(run, fn, …)` | `POST /v1/fuses/check`, raising `FuseTripped` on `stop` |
| `run_verdict(run)` | `runVerdict(run)` | [`GET /v1/fuses/runs/{run}`](http-api.md#get-v1fusesrunsrun) |
| `runs(state, limit)` | `runs({state, limit})` | [`GET /v1/fuses/runs`](http-api.md#get-v1fusesruns) |
| `trip(run, reason)` | `trip(run, {reason})` | [`POST /v1/fuses/runs/{run}/trip`](http-api.md#post-v1fusesrunsruntrip) |
| `reset(run, grant)` | `reset(run, {grant})` | [`POST /v1/fuses/runs/{run}/reset`](http-api.md#post-v1fusesrunsrunreset) |
| `policies()`, `put_policy(…)`, `delete_policy(id)` | `policies()`, `putPolicy(…)`, `deletePolicy(id)` | [`/v1/fuses/policies`](http-api.md#post-v1fusespolicies) |
| `events(after, wait_ms, follow)` — an iterator | `events({after, waitMs, follow})` — an async iterable | [`GET /v1/fuses/events`](http-api.md#get-v1fusesevents) |
| `tape(run, resolve)` | `tape(run, {resolve})` | [`GET /v1/tapes/{run}`](http-api.md#get-v1tapesrun) |
| `replay(run, mode)` → `.openai_base_url`, `.anthropic_base_url`, `.env()`, `.status()`, `.next_tool(name)` | `replay(run, {mode})` → `.openaiBaseUrl`, `.anthropicBaseUrl`, `.env()`, `.status()`, `.nextTool(name)` | [`/v1/replays`](http-api.md#post-v1replays) |
| `diff(a, b)` | `diff(a, b)` | [`GET /v1/diff`](http-api.md#get-v1diff) |
| `recall(query, outcome, limit)` | `recall(query, {outcome, limit})` | [`GET /v1/recall`](http-api.md#get-v1recall) |
| `ledger_head()` | `ledgerHead()` | [`GET /v1/ledger/head`](http-api.md#get-v1ledgerhead) |
| `evidence(since, until)` → `.verify_digest()` | `evidence({since, until})` → `.verifyDigest()` | [`GET /v1/evidence`](http-api.md#get-v1evidence) |
| `otel_env(url, token, tenant)` | `otelEnv(url, {token, tenant})` | — (exporter environment) |
| `request(method, path, query, body)` | `request(method, path, {query, body})` | any route, with the same auth and errors |

Not wrapped: ingest (OpenTelemetry's job), the operator surfaces (credentials,
tenant limits, erasure, ledger anchors and verification, backups), search,
analytics, annotations and evals. `request` reaches any of them with the
client's credential and error mapping.

## Conventions

**Credentials and tenants.** `token` is sent as `Authorization: Bearer`.
`tenant` is sent wherever a route takes one — as `?tenant=` on reads and the
single-run fuse routes, and as a `tenant` body field on the fuse check,
policies and replay creation — and nowhere else, because the server refuses
unknown query parameters. A bound credential is held to its own tenant by
the server whatever is sent. Both clients build from `TRAZA_URL`,
`TRAZA_TOKEN` and `TRAZA_TENANT` (`Client.from_env()` / `Client.fromEnv()`),
the variables the [Claude Code hook](../../examples/guard/README.md) reads,
and neither prints its token in a `repr` or a `console.log`.

**Run ids are percent-encoded as one path segment.** They come from
telemetry, so `/`, `?`, `#`, `%` and spaces are all expected; the tests use
`team/a?x=1#frag %41`.

**Errors map by status.** `401` and `403` are `AuthError` (with the `required`
grant), `404` is `NotFound`, `409` is `Conflict`, `410` is `Gone`, `429` is
`QuotaExceeded` (with `Retry-After` in seconds, and the `quota` object for a
quota or pause), and anything else is `TrazaError`, the base of them all. A
request that gets no response is a `TrazaError` with no status. The machine
kind some routes send (`replay_divergence`, `tool_unreadable`, …) is on
`.type`, and `retryable` is false when the server said `x-should-retry:
false`. Neither client retries on its own.

**`FuseTripped` is not a `TrazaError`.** A `stop` verdict is the answer to the
question the guard asked. Code that catches `TrazaError` to ride out an
unreachable server must not swallow a stop by accident, so the two are
separate types.

**Timestamps are exact.** Every `*_ns` field is Unix nanoseconds, around
1.8e18. Python integers hold that exactly. A JavaScript number does not — a
double is exact only to 2^53, about 9.0e15 — so the TypeScript client reads
responses with its own lossless JSON parser: every `*_ns` integer is a
`bigint`, as is any other integer past 2^53. Recorded payloads — a step's
`request` and `response`, a tool step's `arguments` and `result` — are the
application's own data and are parsed exactly as `JSON.parse` would.

## The guard

The guard asks [`POST /v1/fuses/check`](guard.md#the-ack-is-the-brake--and-it-brakes-the-next-step)
before a step and raises `FuseTripped` on `stop`, so the step never starts. It
is the pre-call half of the fuse design. The verdict on an ingest
acknowledgement is the other half, a backstop that is late by construction;
it reaches whatever exports the spans, which is not these clients.

Two choices are the caller's, not the client's:

- **The run id** must be the session id the agent's instrumentation records
  (`session.id`, `gen_ai.conversation.id`, or OpenLLMetry's association
  property), or the guard and the spans describe different runs.
- **An unreachable server raises by default.** `fail_open=True` (Python) or
  `failOpen: true` (TypeScript) lets the step run instead. Running unguarded
  is a decision someone should make; the shell hook in `examples/guard` makes
  the opposite default for an interactive tool, and says so.

## Replays

`replay(run)` creates a replay and returns its base URLs as **absolute** URLs:
the client's own URL with the server's `base_urls` path appended. They are
concatenated rather than resolved, so a client configured with a reverse
proxy's path prefix keeps it. `env()` returns `OPENAI_BASE_URL`,
`OPENAI_API_KEY`, `ANTHROPIC_BASE_URL` and `ANTHROPIC_API_KEY`; the keys are
the client's token, which the replay routes accept as a bearer token or as
the Anthropic SDK's `x-api-key`. On an open server the keys are a placeholder,
because both provider SDKs refuse to start without one and the server accepts
any.

`next_tool()` / `nextTool()` returns the next recorded tool result, and
`None` / `null` when none is left: running out is how a harness's loop ends.
A replay withdrawn by an erasure still raises `Gone`.

Both clients' READMEs carry a replay loop that was run as written with the
real provider SDKs (`openai` and `anthropic` for Python, `openai` and
`@anthropic-ai/sdk` for TypeScript); the test suites drive the same routes
with plain `urllib` and `fetch`, so they need no provider SDK installed.

## The exporter environment

`otel_env(url, token, tenant)` returns the trace-signal variables:

```python
{'OTEL_EXPORTER_OTLP_TRACES_ENDPOINT': 'http://127.0.0.1:8080/v1/traces',
 'OTEL_EXPORTER_OTLP_TRACES_PROTOCOL': 'http/protobuf',
 'OTEL_EXPORTER_OTLP_TRACES_HEADERS': 'Authorization=Bearer%20trz_…',
 'OTEL_RESOURCE_ATTRIBUTES': 'traza.tenant=acme'}
```

The signal-specific variables, rather than the generic
`OTEL_EXPORTER_OTLP_ENDPOINT` [ingest](ingest.md) shows, because a generic
endpoint would also send metrics and logs to Traza, which ingests traces
only. The header value is percent-encoded, as the OpenTelemetry specification
requires (the space in `Bearer <token>` is `%20`). `traza.tenant` is the
resource attribute [OTLP ingest](http-api.md#post-v1traces) reads a span's
tenant from. The tests decode these variables the way an exporter does and
post an OTLP export with them (OTLP/JSON, which the standard library can
write; the route takes both encodings), and the server stores it under the
tenant named.

## Evidence digests

`verify_digest()` / `verifyDigest()` recomputes an evidence pack's `digest`
from the response body: the SHA-256 of the canonical JSON — keys sorted by
code point, no whitespace — of every field but `digest`. Numbers are
re-emitted exactly as the server printed them, which is the whole difficulty:
Python re-prints `0.00001` as `1e-05`, JavaScript re-prints `2.0` as `2`, and
either would hash different bytes. It is the check in the
[ledger guide](ledger.md#evidence-packs). TypeScript also exports
`verifyEvidenceDigest(text)` for a pack saved to a file. The TypeScript check
uses `crypto.subtle`, which browsers offer only on secure pages.

## Testing

```sh
cargo build --release
python3 -m unittest discover -s sdk/python/tests
(cd sdk/typescript && npm ci && npm test)
```

Each suite spawns `traza-server` — `$TRAZA_SERVER_BIN`, else
`target/release/traza-server` — twice on ephemeral loopback ports, one open
and one with `TRAZA_TOKENS`, each with a private data directory and every
inherited `TRAZA_*` variable removed. A missing binary fails the suite rather
than skipping it. Both run in a few seconds, and `./ci.sh` runs them after the
demos when `python3` and Node 20+ are present.

## Limitations

- **No retries or backoff.** A `429`'s `Retry-After` and a refusal's
  `retryable` are exposed; acting on them is the caller's choice.
- **No connection pooling in Python.** `urllib` opens a connection per
  request. For a guard that asks once per agent step this is noise next to
  the step itself; a tight loop of thousands of calls would feel it.
- **Browsers need the same origin.** The server sends no CORS headers, so a
  page can call it only from the origin that serves both, such as behind one
  reverse proxy.
- **Response types are not validated at runtime.** The dataclasses and
  TypeScript interfaces describe the documented shapes; a field a newer
  server adds is kept in the Python response's `raw` and present on the
  TypeScript object, but nothing checks a response against the types.
- **Node 20 is the documented floor but not exercised here.** The suite runs
  on the Node in use (24 and 25 when this was written); it uses nothing past
  Node 20's APIs, and the client avoids `AbortSignal.any` and
  `AbortSignal.timeout`, which early Node 20 releases lack.
