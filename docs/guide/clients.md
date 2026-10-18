# Client libraries

Two thin clients for the routes an agent harness talks to — the run fuses,
replays and forks, diffs, recall, and the ledger — live in this repository,
with tape-test helpers on top:

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

Both carry the engine's version number, and are tested against the
`traza-server` binary built from the same tree. A version bump that misses
one of them fails the gate (`scripts/check-sdk-metadata.py`, which also holds
each package's copy of `LICENSE` and `NOTICE` to the repository's).

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
| `replay(run, mode)` → `.openai_base_url`, `.anthropic_base_url`, `.env(api_key)`, `.status()`, `.next_tool(name)` | `replay(run, {mode})` → `.openaiBaseUrl`, `.anthropicBaseUrl`, `.env({apiKey})`, `.status()`, `.nextTool(name)` | [`/v1/replays`](http-api.md#post-v1replays) |
| `replay(run, mode, edits, live_after)` | `replay(run, {mode, edits, liveAfter})` | [`POST /v1/replays`](http-api.md#post-v1replays) as a fork |
| `fork(run, at_step, response=… \| tool_result=…, live=True)` — a context manager; `traza.fork(…)` | `fork(run, {atStep, response \| toolResult, live})` → a `Fork` | [`POST /v1/replays`](http-api.md#post-v1replays), then the provider ([forks](#forks-and-tape-tests)) |
| `forks(run, limit)` | `forks(run, {limit})` | [`GET /v1/forks`](http-api.md#get-v1forks) |
| `goldens(limit)`, `mark_golden(run, golden)` | `goldens({limit})`, `markGolden(run, {golden})` | [`GET /v1/goldens`](http-api.md#get-v1goldens), [`POST /v1/annotations`](http-api.md#post-v1annotations) |
| `traza.testing.replay_test(client, run, agent)`, `TapeTest`, pytest-traza | `replayTest(client, run, agent)` | a strict replay, judged ([tape tests](#forks-and-tape-tests)) |
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
unknown query parameters. A bound credential can leave `tenant` unset (the
server scopes it to its own tenant) or name its own tenant; **naming any
other is refused with `403`**, and that includes `""`, which names the
default tenant. Both clients build from `TRAZA_URL`, `TRAZA_TOKEN` and
`TRAZA_TENANT` (`Client.from_env()` / `Client.fromEnv()`), the variables the
[Claude Code hook](../../examples/guard/README.md) reads, and read them the
way it does: a variable set to the empty string counts as unset, so a stray
`TRAZA_TENANT=` does not turn every call of a bound credential into a `403`.
Neither client prints its token in a `repr` or a `console.log`.

**Redirects are refused, not followed.** Traza never redirects its own
routes, so a `3xx` comes from something in front of it — a proxy upgrading
`http` to `https`, or canonicalising a host name. Following it would send the
token wherever the redirect points (Python's `urllib` copies the
`Authorization` header onto the next request, across hosts and from `https`
to `http`). Both clients raise a `TrazaError` with the `3xx` status and the
`Location` in its message instead: configure the URL the redirect names.

**A `request` path must start with `/`.** The path is appended to the
client's URL rather than resolved against it, so a path prefix the URL carries
behind a reverse proxy is kept. A path that does not start with `/` would
continue the URL's host instead: `https://traza.example` and
`.attacker.example/v1/whoami` make a request to another server, token
included, with no redirect involved. Both clients refuse such a path before
anything is sent — `ValueError` in Python, `TypeError` in TypeScript — and
refuse a request URL whose scheme, host or port differs from the client's
before attaching the token. This matters most to code that forwards a route
it was handed.

**Run ids are percent-encoded as one path segment.** They come from
telemetry, so `/`, `?`, `#`, `%` and spaces are all expected; the tests use
`team/a?x=1#frag %41`.

**Errors map by status.** `401` and `403` are `AuthError` (with the `required`
grant), `404` is `NotFound`, `409` is `Conflict`, `410` is `Gone`, `429` is
`QuotaExceeded` (with `Retry-After` in seconds, and the `quota` object for a
quota or pause), and anything else is `TrazaError`, the base of them all. A
request that gets no response is a `TrazaError` with no status. So is an
answer that arrives but cannot be read — a body that is not JSON (a reverse
proxy's sign-in page, a captive portal), JSON that is not the object the
route answers, a body cut short, a status line that is not HTTP — with the
status when one was read and the text that arrived on `.body`. The machine
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
- **A check that cannot be answered raises by default** — an unreachable
  server, a refused check, or an answer that is not a verdict (anything but
  a JSON object whose `verdict` is `continue`, `warn` or `stop`).
  `fail_open=True` (Python) or `failOpen: true` (TypeScript) lets the step
  run instead, in every one of those cases. Running unguarded is a decision
  someone should make; the shell hook in `examples/guard` makes the opposite
  default for an interactive tool, and says so.

## The event feed

`events()` reads [`GET /v1/fuses/events`](http-api.md#get-v1fusesevents) by
cursor. Without `follow` it drains what the server retains and ends at the
first empty read (`wait_ms` / `waitMs` defaults to 0). With `follow` it reads
forever, and **every read is a long-poll**: the wait defaults to 25 s
(`FOLLOW_WAIT_MS`), under both the server's 30 s ceiling and the idle
timeouts common in proxies, and a wait of 0 is refused (`ValueError` /
`RangeError`), because each empty read would be followed at once by the next
— thousands of requests a second against a quiet feed.

## Replays

`replay(run)` creates a replay and returns its base URLs as **absolute** URLs:
the client's own URL with the server's `base_urls` path appended. They are
concatenated rather than resolved, so a client configured with a reverse
proxy's path prefix keeps it. `env()` returns `OPENAI_BASE_URL`,
`OPENAI_API_KEY`, `ANTHROPIC_BASE_URL` and `ANTHROPIC_API_KEY`; the keys are
the `api_key` / `apiKey` passed to it, else the client's own token, which the
replay routes accept as a bearer token or as the Anthropic SDK's `x-api-key`.
On an open server the keys are a placeholder, because both provider SDKs
refuse to start without one and the server accepts any.

**Mind which credential `env()` hands out.** Every library in the process that
receives the environment can read the key, and one that reads
`OPENAI_API_KEY` but builds its own base URL (or ignores `OPENAI_BASE_URL`)
sends it to the real provider. If the client holds an operator's token — the
one that trips runs and writes policies — do not export it. Mint a dedicated
`ro` credential bound to the run's tenant
([`POST /v1/admin/credentials`](http-api.md#post-v1admincredentials)), create
the replay under that tenant (a client with `tenant` set, or the bound
credential itself), and pass the `ro` token as `api_key` / `apiKey`. Any valid
credential of the replay's tenant may consume it; a replay created from the
operator view, with no tenant, answers only unbound operators
([replays follow tenancy](replay.md)).

`next_tool()` / `nextTool()` returns the next recorded tool result, and
`None` / `null` when none is left: running out is how a harness's loop ends.
A replay withdrawn by an erasure still raises `Gone`.

Both clients' READMEs carry a replay loop that was run as written with the
real provider SDKs (`openai` and `anthropic` for Python, `openai` and
`@anthropic-ai/sdk` for TypeScript); the test suites drive the same routes
with plain `urllib` and `fetch`, so they need no provider SDK installed.

## Forks and tape tests

The [fork guide](fork.md) walks through both clients' forks, tape tests and
golden runs; in short:

- **`fork`** creates a fork and switches the agent at the cut. Python's is a
  context manager that points `OPENAI_BASE_URL` and `ANTHROPIC_BASE_URL` at a
  loopback switch — private to the process, answering only the per-fork
  secret it puts in the API-key variables — which forwards to the replay
  until it answers `replay_live` and to the real provider (with the real key,
  read from the environment when the fork was made) after it; with the
  OpenTelemetry SDK configured it also tags the spans started inside the
  block. TypeScript's returns a `Fork` whose `openaiOptions()` /
  `anthropicOptions()` carry a switching `fetch` for the provider SDKs, and
  whose `spanProcessor()` tags spans until `close()`. `tool_result(name)` /
  `toolResult(name)` returns the edited or recorded tool step, or
  `None` / `null` once the real tool should run.
- **Tape tests** replay a run strictly and fail — `TapeMismatch`, an
  `AssertionError` in Python, an `Error` in TypeScript — naming where the
  agent left the recorded path, when it stopped early, or when it overran.
  Python adds the **pytest-traza** plugin (`@pytest.mark.traza_tape("run")`,
  registered as a `pytest11` entry point; it needs pytest, the package still
  depends on nothing) and the **`traza`** command:

  ```sh
  traza test -- python agent.py --task "{input}"   # every golden run; JUnit XML on stdout
  traza golden add weather-7                        # mark a run golden
  traza forks weather-7                             # its branches and where they parted
  traza fork weather-7 --at 1 --response "Sunny." -- python agent.py
  ```

  The command reads the server from `--url` or `TRAZA_URL`, the tenant from
  `--tenant` or `TRAZA_TENANT`, and the credential from `TRAZA_TOKEN` only.
  It exits 0 when every test passed, 1 when one failed (or there was nothing
  to test, unless `--allow-empty`), 2 when the server could not be asked.

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

The fork tests stand a canned-answer stub in for the real provider, so the
hand-over at the cut is observed — which request reached it, with which key
— and nothing leaves the machine. The pytest-traza test runs pytest in a
child interpreter and is skipped, saying so, when pytest is not installed;
the OpenTelemetry tagging test likewise needs the OpenTelemetry SDK, and the
span processor's own logic is tested without it.

Each suite spawns `traza-server` — `$TRAZA_SERVER_BIN`, else
`target/release/traza-server` — twice on ephemeral loopback ports, one open
and one with `TRAZA_TOKENS`, each with a private data directory and every
inherited `TRAZA_*` variable removed. A missing binary fails the suite rather
than skipping it. Answers the real server never gives — redirects, HTML
pages, truncated bodies — come from a small canned-answer stub beside each
suite, and the TypeScript suite also drives its own harness's failure paths
(a server that never listens, exits first or cannot be executed), which must
end the test process promptly rather than hang it. The Python suite runs its
own module fixture in a child interpreter with a seed request refused, and
again with the second server failing to start, and checks that no server or
data directory outlives either. Both run in a few seconds. `./ci.sh` runs
`scripts/check-sdk-metadata.py` and the Python suite after the demos, and the
TypeScript suite when Node 20+ with npm is present.

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
