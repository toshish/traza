# HTTP API reference

Every route Traza serves. Responses are JSON unless noted. All examples were
run against a live server; the response bodies shown are real output.

## Conventions

**Base URL.** `http://HOST:PORT`, default `http://127.0.0.1:8080`. TLS is
reverse-proxy territory — Traza speaks plain HTTP.

**Authentication.** Once any credential exists — from `TRAZA_TOKENS`, or
minted at runtime with [`POST /v1/admin/credentials`](#post-v1admincredentials)
— every `/v1` request needs `Authorization: Bearer TOKEN`. With neither, the
server is open (on loopback by default). A `ro` credential may `GET` (and
`POST /v1/fuses/check`, the one read that carries a body); an `rw` credential
may `GET` and `POST`; `admin` adds erasure, eval tombstones,
backups, ledger anchors and the `/v1/admin/*` routes. A credential may be **bound** to one
tenant (`scope@tenant:token` in `TRAZA_TOKENS`, `"tenant"` when minted): a
bound credential reads and writes only its tenant's data, on every surface:
search, export, tail, analytics, annotations, payload fetch, evals, erasure,
usage, and MCP alike. The binding is applied where queries are constructed, so
no endpoint can forget it. Consequences worth knowing before they surprise
you:

- Naming a foreign tenant in any `tenant=` parameter answers
  `403 {"error":"this credential is bound to a different tenant","required":"unbound"}`
  — the caller knows what it asked for, so the refusal says why.
- The store-global operator endpoints — `/v1/stats`, `/v1/metrics`,
  `/v1/metrics.json`, `/v1/verify`, `/v1/checkpoint`, `/v1/flush`,
  `/v1/backups/*`, `/v1/ledger`, `/v1/ledger/verify`, and
  `/v1/ledger/anchors` — answer bound credentials
  `403 {"error":"forbidden","required":"unbound"}`: they report and move the
  whole store's data, and even the volumes disclose co-tenants. A bound
  credential's accounting surfaces are [`GET /v1/usage`](#get-v1usage) and
  [`GET /v1/tenants`](#get-v1tenants).
- **Every path under `/v1/admin/` is unbound-admin only**, decided by the
  prefix rather than a list of routes, so a bound or non-admin credential is
  refused there before any route is consulted — including a path that does
  not exist.
- Every `403` from the auth gate carries `"required"`: `"rw"`, `"admin"` or
  `"unbound"`, naming the capability the credential lacked, so a client can
  say which grant would have worked.
- **In open mode, admin operations refuse cross-site browser requests.** With
  no credential configured, erasure, eval tombstones, backups and every path
  under `/v1/admin/` answer a request whose `Origin` is not a loopback page
  with `403 {"error":"origin not allowed: …","next":"…"}`. Without it, any web
  page the operator visits could mint a credential or erase data through the
  operator's browser with a plain cross-site `POST`. Requests with no `Origin`
  (`curl`, scripts) and pages served from loopback (the dashboard) are
  unaffected, and once a credential exists the rule is not applied — a page
  cannot attach a bearer token it does not know. This `403` carries `next`
  rather than `required`: no grant is missing.
- **Open mode ends for requests already in flight.** Authentication is
  decided from a request's headers, before its body is read. A request whose
  headers arrived while the store was open, but whose body arrived after the
  store's first credential was minted, is authenticated again once its body
  is in: without a credential it is `401`, exactly as if it had arrived after
  the bootstrap. Of two bootstraps racing, exactly one creates a credential.
- **Admin request bodies must be declared JSON.** A `POST` under `/v1/admin/`
  that carries a body must send `Content-Type: application/json`, or it is
  `415`. An HTML form cannot send that type, and a cross-site script cannot
  send it without a CORS preflight this server never approves.

[`GET /v1/whoami`](#get-v1whoami) tells any caller what its own credential
is. See [administration](../operations/administration.md#authentication).

**Timestamps** are integer Unix nanoseconds everywhere, in both directions.

**Unknown query parameters are rejected**, not ignored:
`400 {"error":"unknown query parameter: bogus"}`. A typo fails loudly rather
than silently returning the wrong rows.

**Connections persist.** HTTP/1.1 keep-alive is the default. `GET /v1/export`
is the one route that always closes its connection, because it is chunked with
trailers and has no declared length.

**Limits.** Request bodies are capped at 64 MiB, request headers at 64 KiB.
Ingest batches are additionally held to the admission caps (attributes,
key length, events, stored span size, spans per batch; see
[configuration](../configuration.md#server-flags)).

**Compressed bodies.** `POST /v1/spans` and `POST /v1/traces` accept
`Content-Encoding: gzip` (or `x-gzip`) — what an OpenTelemetry exporter sends
with compression on. The body is decompressed by the server's own inflate
(RFC 1951/1952, no dependency), every member's CRC-32 and length checked, with
the decompressed size capped at the same 64 MiB as an uncompressed body: past
it the request is refused with `413` before more is produced, so a compressed
request can hold at most the compressed body plus 64 MiB in memory while it is
decoded. A damaged body is `400` naming the damage
(`invalid gzip body: CRC-32 mismatch`). Every other route, and every other
coding (`br`, `deflate`, `zstd`, a doubled `gzip`), is `415`.

### Status codes

| Code | Meaning |
|---|---|
| `200` | Success |
| `201` | Created: a backup pin, a credential, a replay, a ledger anchor |
| `400` | Malformed body, invalid parameter, a violated ingest invariant, or an exceeded admission cap |
| `401` | Missing, malformed, unknown, expired, or revoked bearer token — including a request admitted in open mode whose body arrived after the store's first credential. Carries `WWW-Authenticate: Bearer` |
| `403` | Valid token without the grant this request needs — the body's `"required"` names it (`rw`, `admin`, `unbound`) — or a tenant-bound credential naming a foreign tenant; in open mode, also a browser request from a non-loopback page to an admin operation |
| `404` | No such route, or the requested trace/session/payload/dataset/experiment/credential/replay does not exist. Cross-tenant reads answer `404`, never `403` — existence is a fact about another tenant |
| `409` | A conflict a retry may resolve: a mutation for a tenant whose erasure is pending, a tombstoned parent version, a payload reference not in the store, a pin label already taken, a credential token already registered, an environment credential named for revocation, a first credential that is not an unbound admin, a `/v1/fuses/*` route on a server started with `--no-fuses`, an anchor whose `target_head` is not the recorded head. Also a run that cannot be replayed, a strict replay's divergence, and a replayed answer too large to render — see [replays](#post-v1replays) for which of those a retry cannot fix |
| `410` | A tombstoned dataset version: the content is withheld, the fact of the deletion is not. Also a replay that is exhausted or that an erasure withdrew |
| `413` | A gzip body that decompresses past the 64 MiB body cap |
| `415` | A `Content-Encoding` other than `gzip` on an ingest route, or any coding elsewhere; a `/v1/admin/` body not declared `application/json` |
| `429` | An ingest batch refused by its tenant's monthly quota or ingest pause, or a replay refused for want of room. Carries `Retry-After` |
| `503` | The store could not serve the request, or the server is at its connection limit. Retry with backoff |

## Route index

| Method | Path | Purpose |
|---|---|---|
| `POST` | [`/v1/spans`](#post-v1spans) | Ingest a native JSON span batch |
| `POST` | [`/v1/traces`](#post-v1traces) | Ingest OTLP/HTTP (protobuf or JSON) |
| `POST` | [`/v1/flush`](#post-v1flush) | Seal buffered spans into a segment |
| `POST` | [`/v1/checkpoint`](#post-v1checkpoint) | Publish a generation |
| `POST` | [`/v1/backups/{label}`](#post-v1backupslabel) | Pin and verify a backup (unbound `admin`) |
| `POST` | [`/v1/backups/{label}/release`](#post-v1backupslabelrelease) | Release a pin (unbound `admin`) |
| `GET` | [`/v1/verify`](#get-v1verify) | Verify the live generation |
| `POST` | [`/v1/erasures`](#post-v1erasures) | Erase a trace, span, session, payload, or whole tenant |
| `GET` | [`/v1/erasures`](#get-v1erasures) | The tombstone log |
| `GET` | [`/v1/erasures/{id}/verify`](#get-v1erasuresidverify) | The erasure receipt |
| `GET` | [`/v1/ledger`](#get-v1ledger) | Ledger entries, paged |
| `GET` | [`/v1/ledger/head`](#get-v1ledgerhead) | The ledger's tip |
| `GET` | [`/v1/ledger/verify`](#get-v1ledgerverify) | Verify the chain, and the live generation against it |
| `POST` | [`/v1/ledger/anchors`](#post-v1ledgeranchors) | Record an external notary's anchor (`admin`) |
| `GET` | [`/v1/evidence`](#get-v1evidence) | The evidence pack |
| `GET` | [`/v1/spans`](#get-v1spans) | Filtered span search |
| `GET` | [`/v1/traces/{trace_id}`](#get-v1tracestrace_id) | One trace's spans and annotations |
| `GET` | [`/v1/sessions`](#get-v1sessions) | Sessions, most recent activity first |
| `GET` | [`/v1/sessions/{id}`](#get-v1sessionsid) | One session's rollup and per-trace breakdown |
| `GET` | [`/v1/stats/llm`](#get-v1statsllm) | Token and cost aggregation |
| `GET` | [`/v1/stats/series`](#get-v1statsseries) | Volume, errors, tokens, cost and latency over time |
| `GET` | [`/v1/stats/duration`](#get-v1statsduration) | Duration distribution and percentiles |
| `GET` | [`/v1/stats/failures`](#get-v1statsfailures) | Errors grouped by signature |
| `GET` | [`/v1/stats/slowest`](#get-v1statsslowest) | The slowest matching spans |
| `GET` | [`/v1/tapes/{run}`](#get-v1tapesrun) | A run's steps in order, with inputs, outputs and digests |
| `POST` | [`/v1/replays`](#post-v1replays) | Snapshot a run for replay |
| `GET` | [`/v1/replays/{id}`](#get-v1replaysid) | What a replay has served |
| `POST` | [`/v1/replays/{id}/openai/v1/chat/completions`](#post-v1replaysidopenaiv1chatcompletions) | The next recorded model answer, as an OpenAI chat completion |
| `POST` | [`/v1/replays/{id}/anthropic/v1/messages`](#post-v1replaysidanthropicv1messages) | The next recorded model answer, as an Anthropic message |
| `POST` | [`/v1/replays/{id}/tools/next`](#post-v1replaysidtoolsnext) | The next recorded tool result |
| `GET` | [`/v1/diff`](#get-v1diff) | Two runs aligned, and where they first diverge |
| `GET` | [`/v1/recall`](#get-v1recall) | Earlier runs containing the same words, successes first |
| `POST` | [`/v1/annotations`](#post-v1annotations) | Attach a score or feedback record |
| `GET` | [`/v1/annotations`](#get-v1annotations) | Query annotations |
| `POST` | [`/v1/datasets`](#post-v1datasets) | Create a dataset |
| `GET` | [`/v1/datasets`](#get-v1datasets) | List datasets with version summaries |
| `GET` | [`/v1/datasets/{id}`](#get-v1datasetsid) | One dataset with version summaries |
| `POST` | [`/v1/datasets/{id}/versions`](#post-v1datasetsidversions) | Create a content-addressed dataset version |
| `GET` | [`/v1/datasets/{id}/versions/{vid}`](#get-v1datasetsidversionsvid) | One version's manifest and example bodies |
| `POST` | [`/v1/datasets/{id}/versions/{vid}/tombstone`](#post-v1datasetsidversionsvidtombstone) | Logically delete a version (`admin`) |
| `POST` | [`/v1/experiments`](#post-v1experiments) | Create an experiment against a dataset version |
| `GET` | [`/v1/experiments`](#get-v1experiments) | List experiments |
| `GET` | [`/v1/experiments/{id}`](#get-v1experimentsid) | One experiment with derived state |
| `POST` | [`/v1/experiments/{id}/runs`](#post-v1experimentsidruns) | Record a task run |
| `GET` | [`/v1/experiments/{id}/runs`](#get-v1experimentsidruns) | An experiment's recorded runs |
| `GET` | [`/v1/experiments/{id}/scores`](#get-v1experimentsidscores) | An experiment's scores |
| `GET` | [`/v1/experiments/{id}/summary`](#get-v1experimentsidsummary) | Per-name score distributions |
| `GET` | [`/v1/experiments/diff`](#get-v1experimentsdiff) | Compare two experiments' scores |
| `GET` | [`/v1/tenants`](#get-v1tenants) | Per-tenant usage accounting |
| `GET` | [`/v1/usage`](#get-v1usage) | Per-tenant ingest counters, limits and quota state |
| `GET` | [`/v1/whoami`](#get-v1whoami) | What the calling credential is |
| `POST` | [`/v1/admin/credentials`](#post-v1admincredentials) | Mint or register a credential (unbound `admin`) |
| `GET` | [`/v1/admin/credentials`](#get-v1admincredentials) | List credentials (unbound `admin`) |
| `POST` | [`/v1/admin/credentials/{id}/revoke`](#post-v1admincredentialsidrevoke) | Revoke a credential (unbound `admin`) |
| `POST` | [`/v1/admin/tenants/{tenant}/limits`](#post-v1admintenantstenantlimits) | Replace a tenant's quotas, pause and retention (unbound `admin`) |
| `GET` | [`/v1/admin/tenants/{tenant}/limits`](#get-v1admintenantstenantlimits) | A tenant's limits (unbound `admin`) |
| `POST` | [`/v1/fuses/check`](#post-v1fusescheck) | Pre-call gate: may a run take its next step |
| `GET` | [`/v1/fuses/runs/{run}`](#get-v1fusesrunsrun) | One run's fuse verdict, usage and limits |
| `GET` | [`/v1/fuses/runs`](#get-v1fusesruns) | Runs the guard holds, by verdict |
| `POST` | [`/v1/fuses/runs/{run}/trip`](#post-v1fusesrunsruntrip) | The kill switch |
| `POST` | [`/v1/fuses/runs/{run}/reset`](#post-v1fusesrunsrunreset) | Clear a stop, optionally granting higher limits |
| `POST` | [`/v1/fuses/policies`](#post-v1fusespolicies) | Create or replace a fuse policy |
| `GET` | [`/v1/fuses/policies`](#get-v1fusespolicies) | Fuse policies and the global default |
| `POST` | [`/v1/fuses/policies/{id}/delete`](#post-v1fusespoliciesiddelete) | Delete a fuse policy |
| `GET` | [`/v1/fuses/events`](#get-v1fusesevents) | Fuse transitions by cursor, with long-poll |
| `GET` | [`/v1/payloads/{reference}`](#get-v1payloadsreference) | Raw bytes of an offloaded payload |
| `GET` | [`/v1/export`](#get-v1export) | Streaming NDJSON export |
| `GET` | [`/v1/tail`](#get-v1tail) | Live span stream, in admission order |
| `GET` | [`/v1/stats`](#get-v1stats) | Store statistics |
| `GET` | [`/v1/metrics`](#get-v1metrics) | Prometheus text metrics |
| `GET` | [`/v1/metrics.json`](#get-v1metricsjson) | The same metrics as JSON |
| `GET` | [`/healthz`, `/readyz`](#get-healthz-and-readyz) | Liveness and readiness probes (no auth) |
| `POST` | [`/v1/mcp`](#post-v1mcp) | Model Context Protocol endpoint (off unless `--mcp`) |
| `GET` | [`/`, `/dashboard`](#get--and-dashboard) | The trace browser |

---

## Ingest

### `POST /v1/spans`

Ingests a batch of spans in Traza's native JSON.

**Body.** Either a bare array of spans or `{"spans": [...]}`. Field semantics
are in the [data model](data-model.md#the-span).

**Response `200`.**

```json
{"accepted":1,"durability":"wal"}
```

`accepted` is the number of spans **stored** — it is a durability claim, and
it counts nothing else. `durability` is `buffered`, `wal`, or `flushed` and
states what the acknowledgement guarantees. While an
[erasure](#erasure) is pending, spans it covers are acknowledged and
deliberately not stored; the response then carries the split explicitly:

```json
{"accepted":1,"suppressed":1,"durability":"wal"}
```

`suppressed` appears only when nonzero.

**Run fuses.** When the batch touched a run whose [fuse](guard.md) verdict is
not `continue`, the response names it, and a `Traza-Fuse` header carries the
most severe verdict in the batch:

```
HTTP/1.1 200 OK
Traza-Fuse: stop; run=sess-7f3a

{"accepted":1,"durability":"wal","fuses":[{"reasons":["max_repeats 4 reached: tool:search_orders#465a6bab9a1c447d repeated 4 times"],"run":"sess-7f3a","tenant":"","verdict":"stop"}]}
```

`fuses` and the header are absent when nothing is to be reported. The run id
in the header is percent-encoded outside visible ASCII and in `%`, `;`, `,`,
`"` and space, because it comes from telemetry. The spans are stored either
way — a verdict is a statement about the run's NEXT step, never a refusal of
the batch it rides on; see [the guide](guard.md#the-ack-is-the-brake--and-it-brakes-the-next-step).

**Tenancy.** A span may carry a top-level `$tenant` field: lowercase
`[a-z0-9][a-z0-9._-]`, at most 64 bytes. Empty or absent is the **default
tenant**, and it is never serialized back — a single-tenant deployment writes
byte-identical records to what it wrote before tenancy existed. The tenant is
part of the primary key `(tenant, trace_id, span_id)`, so two tenants sharing
a trace id can never upsert over each other. A tenant-bound credential stamps
its binding onto spans that name no tenant, and a span claiming a *different*
tenant fails the **whole batch** with `400` — loudly and batch-atomically,
because silently rewriting it would hide a misconfigured exporter forever.

The key is `$tenant`, not `tenant`, and the `$` earns its place: a span's
top-level namespace is open, so a bare `tenant` is client data preserved
verbatim (it is not, and never becomes, an identity). Reserving `$tenant` as
`$payload` is reserved means a store written before tenancy reads back
correctly — its bare `tenant` values stay data — rather than a value being
promoted to an identity no query selects and no erasure names. The `?tenant=`
read filter and the erasure subject's `tenant` field are their own closed
namespaces and keep the plain name.

**Errors.**

| Response | Cause |
|---|---|
| `400 {"error":"body must be an array or {spans: [...]}"}` | Body's first non-whitespace byte is neither `[` nor `{` |
| `400 {"error":"missing field \`name\` at line 1 column 31"}` | A required field is absent; serde names it |
| `400 {"error":"span 0: trace_id is empty"}` | Empty primary-key half, at batch index 0 |
| `400 {"error":"span 0: span_id is empty"}` | As above |
| `400 {"error":"span 0: tenant does not match the credential's binding"}` | A bound credential's span claims another tenant |
| `400 {"error":"tenant must be lowercase [a-z0-9][a-z0-9._-], at most 64 bytes"}` | An inadmissible tenant identity, refused by the engine |
| `400 {"error":"span 0 (trace t1, span s1): 1500 attributes exceeds the max-span-attributes cap of 1024"}` | An admission cap; the message names the cap and the span (index, trace id, span id) |
| `400 {"error":"batch of 120000 spans exceeds the max-batch-spans cap of 100000"}` | Too many spans in one request |
| `429 {"error":"quota exceeded: monthly_spans for tenant acme","quota":{…}}` | The tenant's monthly quota or ingest pause; see [quota refusals](#quota-refusals) |
| `503 {"error":"…"}` | The store rejected the write |

Validation is atomic per batch: one invalid span stores none of them. The
admission caps are checked before anything is written — before payload
offload too, so a refused span leaves no payload file behind.

#### Quota refusals

A batch that would take its tenant's month past `monthly_spans` or
`monthly_bytes`, or that belongs to a tenant whose ingest is paused (see
[tenant limits](#post-v1admintenantstenantlimits)), is refused **whole** with
`429 Too Many Requests` and a `Retry-After` header: the seconds until the
first instant of next month (UTC) for a monthly limit, `60` for a pause.

```json
{"error":"quota exceeded: monthly_spans for tenant acme",
 "quota":{"tenant":"acme","dimension":"monthly_spans","limit":1000000,
          "used":999990,"window":"2026-10","resets_at":"2026-11-01T00:00:00Z"}}
```

| Field | Description |
|---|---|
| `tenant` | The tenant whose limit refused the batch (`""` is the default tenant) |
| `dimension` | `monthly_spans`, `monthly_bytes`, or `paused` |
| `limit` | The limit's value; `null` for a pause |
| `used` | The month's usage in that dimension before this batch (spans, for a pause) |
| `window` | The month, `YYYY-MM` (UTC) |
| `resets_at` | When the month resets (RFC 3339); `null` for a pause |

A batch is refused when its spans or bytes **would** exceed the limit, so a
batch that lands exactly on it is accepted and the next one is refused. A
batch mixing tenants (possible only for an unbound credential) is refused
whole if any tenant in it is over. Reads, exports and erasure are never
blocked by a quota. Concurrent batches are checked against the counters as
they stand, so batches in flight together can overshoot a limit by at most
their own size.

### `POST /v1/traces`

OTLP/HTTP ingest. `Content-Type: application/x-protobuf` selects the binary
decoder; anything else is parsed as OTLP/HTTP JSON.

**Response `200` (JSON request).**

```json
{"partialSuccess":{}}
```

Spans a pending [erasure](#erasure) suppressed were acknowledged and not
stored, and the response says so in OTLP's own vocabulary:
`{"partialSuccess":{"rejectedSpans":N,"errorMessage":"…"}}`.

**Response `200` (protobuf request).** `Content-Type: application/x-protobuf`
with a zero-length body — the encoding of an empty `ExportTraceServiceResponse`.
With suppressions, the body encodes
`partial_success { rejected_spans, error_message }` instead, for the same
reason: an empty response is a claim of full success, and it would be false.

**Run fuses.** OTLP's response schema has no place for a verdict, so both
encodings carry only the `Traza-Fuse` header described under
[`POST /v1/spans`](#post-v1spans), with the body unchanged.

**Tenancy.** OTLP has no top-level tenant field, so Traza reads the
`traza.tenant` **resource attribute**: set it on the exporter's resource and
every span in the export lands under that tenant. A bound credential stamps
and checks it exactly as native ingest does, and an invalid `traza.tenant` is
a `400` for the **whole export**, deliberately loud — OTLP's `partialSuccess`
is for data the server chose to drop, not for a defect the client must fix.

**Errors.** `400` with the decode failure for malformed protobuf or JSON, a
non-hex id, or a timestamp that is not a `u64`; `400` for an exceeded
admission cap, exactly as native ingest words it. `429` with the same JSON
body and `Retry-After` as [native ingest](#quota-refusals) when a quota or
pause refuses the export — for a protobuf request too, since an OTLP exporter
retries on `429` with `Retry-After` and the body is for whoever reads its
logs. `503` if the store rejected the write.

The full field mapping is in [ingest](ingest.md#otlp-mapping).

### `POST /v1/flush`

Seals every currently buffered span into a durable segment.

```json
{"flushed":true}
```

Errors: `503` if the seal failed.

### `POST /v1/checkpoint`

Publishes a generation: seals the buffer, writes a manifest naming every
load-bearing file with its digest, and moves `CURRENT`. The server also does
this every five minutes. See [backup and restore](../operations/backup.md).

```json
{"generation":42}
```

Errors: `503` if the checkpoint failed. Nothing is published unless it
succeeds — the prior generation stays live.

### `POST /v1/backups/{label}`

Checkpoints, hard-links the manifested files into `pins/{label}`, and verifies
every digest before reporting success. The server keeps running throughout;
the pin holds its bytes even after compaction unlinks the originals, so the
copy can proceed at its own pace.

```json
{"backup":"nightly","generation":42,
 "path":"/var/lib/traza/pins/nightly","verified":true}
```

`path` is a directory to copy — Traza never writes outside its own data
directory. Copy it, then release the pin.

**Needs an unbound `admin` credential.** A pin holds every byte of its
generation on disk until it is released, so taking one is a capacity
decision rather than a write; `rw` gets `403` with `"required":"admin"`, and a
bound credential `"required":"unbound"`.

The pin carries the telemetry — segments, the annotation, eval and tombstone
logs, payload bytes — and **not** `credentials.jsonl`, `limits.json` or
`usage.json`: a backup never carries credential digests, and restoring one
never resurrects a revoked token, rolls back a tenant's limits, or rewinds
what it has been charged for.

Errors: `409` if a pin by that label already exists or the label is not a
single non-hidden path component; `500` with a `problems` array if the pin does
not verify; `503` if the checkpoint or linking failed.

### `POST /v1/backups/{label}/release`

Removes a pin, freeing the disk its unshared bytes were holding. Idempotent.
Unbound `admin`, like taking one.

```json
{"released":true,"backup":"nightly"}
```

### `GET /v1/verify`

Re-reads and re-digests every file in the live generation's manifest.

```json
{"generation":42,"intact":true,"problems":[]}
```

`problems` names each discrepancy — `segment-….seg: digest mismatch`,
`payloads/ab/cd.bin: missing` — because which file is damaged is what decides
whether to restore. A read, so an `ro` token may ask.

---

## Query

### `GET /v1/spans`

Filtered span search, ordered by Traza's stable span order
(`start_time_ns`, `end_time_ns`, `tenant`, `trace_id`, `span_id`) — the
tenant joined the ordering key when it joined the primary key.

**Parameters.** All filters are ANDed. All values are URL-encoded.

| Parameter | Type | Description |
|---|---|---|
| `service` | string | Exact match on the emitting service |
| `name` | string | Exact match on the operation name |
| `status` | string | Exact match on the span's own `status` field |
| `not_status` | string | Exclude spans with this status. Repeatable |
| `attr.KEY` | JSON or string | Exact attribute match. Repeatable — each occurrence adds a condition |
| `content` / `q` | string | Spans whose text contains **every word** given. See below |
| `session` | string | Every span of a session, unioning all recognized session keys |
| `tenant` | string | Only spans of this tenant. **An empty value names the default tenant explicitly; omitting the parameter matches every tenant** (the operator view). A bound credential has its own tenant forced here. Also accepted by `/v1/export`, `/v1/tail`, and every `/v1/stats/*` route |
| `cursor` | string | Opaque token from a previous response's `next_cursor`. Returns the page after it |
| `not_attr.KEY` | JSON or string | Exclude spans whose attribute equals this. A span missing the key entirely is **kept**. Repeatable |
| `min_attr.KEY` | number | Attribute is at least this, compared numerically. Repeatable |
| `max_attr.KEY` | number | Attribute is at most this, compared numerically. Repeatable |
| `min_duration_ms` | integer | Minimum `end - start`, in milliseconds |
| `min_duration_ns` | integer | Minimum `end - start`, in nanoseconds |
| `max_duration_ms` | integer | Maximum `end - start`, in milliseconds |
| `max_duration_ns` | integer | Maximum `end - start`, in nanoseconds |
| `since` / `since_ns` | integer | Only spans starting at or after this Unix-nanosecond timestamp |
| `until` / `until_ns` | integer | Only spans starting at or before this timestamp |
| `sort` | string | `duration` / `-duration` / `start` / `-start` (a leading `-` is descending). Omit for Traza's stable order |
| `limit` | integer | Maximum spans returned. **Default 100**, applied after filtering |

#### Content search: `content=`

`content=refund` returns spans whose text contains the word *refund*. The text
searched is every string value in the span's attributes and its events'
attributes, plus event names — so a prompt, a completion, a tool call's
arguments and a nested message array are all covered.

```
GET /v1/spans?content=refund&since=1700000000000000000&limit=50
```

**It is word search, not substring search, and not a phrase search.** Those
three differences are the ones that surprise people, so they are worth stating
directly:

| Query | Matches `Refund the order` | Matches `refunds were issued` |
|---|---|---|
| `content=refund` | yes | **no** |
| `content=refunds` | no | yes |
| `content=refund order` | yes (both words present) | no |

- Case and punctuation are ignored: `refund`, `Refund`, `"refund"` and
  `refund,` are the same word.
- There is no stemming. `refund` and `refunds` are different words.
- A multi-word query is a **conjunction**: every word must appear somewhere in
  the span, in any order. `content=refund order` does not require them
  adjacent.
- Words are runs of ASCII letters and digits. Text in other scripts is not
  tokenized, so `content=世界` matches nothing.
- **An offloaded value is searchable only within its 256-character preview.**
  A string attribute longer than `--payload-threshold-bytes` is moved to the
  payload store at ingest — before anything indexes the span — and replaced
  inline by a reference carrying a preview. Only that preview is searchable.
  At the 256 KiB default almost nothing is offloaded; lower the threshold and
  this becomes the rule rather than the exception. Search is bounded here, not
  wrong: the index and the match are computed from the same text, so a span is
  never skipped that would have matched.

This is not an arbitrary restriction — it is what the index can answer
*correctly*. A word index cannot soundly drive a substring search: it would
have to skip the span reading `refunds were issued` when you searched
`refund`, and skipping it is a wrong answer rather than a slow one. See
[the segment format](../segment-format.md#the-content-index).

To find spans by the *whole value* of an attribute rather than a word inside
it, use `attr.KEY=VALUE`, which is exact and separately indexed.

Content search composes with every other parameter, and is fast when it is
selective — see [capacity](../operations/capacity.md#content-search) for
measured latencies, including the case where it cannot help.

**`status=` reads the span's own field; `attr.status=` reads an attribute.**
These are different filters and the difference used to be a trap: every
aggregate in the store counts errors from `Span::status`, but nothing could
*select* on it, so the natural-looking `attr.status=error` matched an attribute
most instrumentation never writes and returned an empty array indistinguishable
from "no errors". Use `status=error` for failures and `not_status=error` for
their complement. `not_status` has no missing-key subtlety — every span has a
status, and an empty one is a value like any other.

`attr.KEY=VALUE` **matches a scalar regardless of how it was typed on the
wire.** `attr.code=200` finds spans that stored the number `200` and spans
that stored the string `"200"`. This matters because instrumentation is
inconsistent about it — several SDKs stringify token counts and status codes —
and the previous behaviour matched only the number, so a store full of
stringified values answered every such query with an empty array that was
indistinguishable from "no such data". Matching is still exact: `200` does not
match `20`, and containers (arrays, objects) compare structurally.

`min_attr.KEY` / `max_attr.KEY` read the attribute as a number whether it was
stored as one or as a string, so `min_attr.llm.usage.total_tokens=1000` works
across both conventions. A span whose attribute is absent or non-numeric does
not match a numeric bound.

`not_attr.KEY` deliberately **keeps** spans that lack the key:
`not_attr.status=error` means "not known to be an error", which includes spans
that never recorded a status. Treating a missing key as excluded would hide
most of a corpus behind a filter that reads like it only removes failures.

**Sorting costs a full scan.** Without `sort`, a query stops as soon as it has
`limit` matches. With it, every match must be found before any can be ranked,
so `sort` is refused with `400` past an internal candidate ceiling — narrow
with a time range, `service`, or an attribute and retry. Returning the "ten
slowest" out of an arbitrary first page would be a wrong answer that looks
like a right one.

**Compute is budgeted.** Every span query and aggregation — this route,
[`GET /v1/traces/{id}`](#get-v1tracestrace_id), the `/v1/stats/*` family,
sessions, `/v1/tenants`, and the reachability probe behind a tenant-bound
[`GET /v1/payloads/{ref}`](#get-v1payloadsreference) — runs under the
server's [`--query-deadline-ms`](../configuration.md#server-flags) budget
(default 30 s). A request still computing past it answers
`400 {"error":"query deadline exceeded: ..."}` with **no rows**, naming the
elapsed time and the narrowing that would help. Like the sort ceiling it is
a `400`, not a `503`: retrying an over-budget query without narrowing cannot
help, so "retry with backoff" would be a lie. The streams are exempt —
[`GET /v1/export`](#get-v1export) and [`GET /v1/tail`](#get-v1tail) are
documented unbounded, and the socket timeout bounds their abandonment — and
so is every erasure read, because a partial receipt is worse than a slow
one. `traza_query_deadline_exceeded_total` counts these refusals.

**Time ranges skip a segment's records.** `since` / `until` are compared
against each segment's stored timestamp range, so a segment that cannot
contain a
match has none of its records read. `traza_segments_pruned_by_time_total` and
`traza_segments_examined_total` in [`/v1/metrics`](../operations/monitoring.md)
show how much of the store a given window is eliminating. Segments written
before v0.18 carry no range and are always scanned; they age out through
normal compaction.

The work avoided is real and counter-verified, but **no latency figure is
published for it yet**: the only measurement attempted so far ran on a
40-segment store, which is too small for per-segment cost to be visible above
noise. Expect it to matter in proportion to segment count, like compaction.

`session=` is not the same as `attr.session.id=`: it unions every recognized
session key, so a session whose spans use mixed conventions returns whole.

**Paging is by cursor, not by growing `limit`.** A response whose page came
back full carries `next_cursor`; passing it back returns the spans strictly
after the last one, in the same total order. Re-requesting with a larger
`limit` instead re-reads and re-sends every row already in hand, which makes
scrolling quadratic in the number of pages. A short page carries
`next_cursor: null` — it has already reached the end, and a cursor there could
only return nothing. The token is opaque; a malformed one is a `400` rather
than a silently wrong page. The token format changed when the tenant joined
the ordering key — a leading version byte now identifies the layout — so a
pre-tenancy cursor parses as **invalid** (`400 {"error":"cursor is not a token
this server issued"}`), never as a plausible wrong position. Re-issue the
query without a cursor and page afresh.

**Response `200`.** An envelope: the spans, the cursor, and what the query
cost.

```sh
curl 'http://localhost:8080/v1/spans?service=checkout&attr.region=us-east&min_duration_ms=2&limit=50'

# The ten slowest failed calls in a window that cost over a cent
curl 'http://localhost:8080/v1/spans?since_ns=1700000000000000000&until_ns=1700003600000000000&status=error&min_attr.llm.cost_usd=0.01&sort=-duration&limit=10'
```

```json
{"spans":[{"attributes":{"http.method":"POST","region":"us-east"},"end_time_ns":1700000000002500000,"events":[],"name":"charge","parent_span_id":null,"service":"checkout","span_id":"span-1","start_time_ns":1700000000000000000,"status":"ok","trace_id":"trace-1"}],"next_cursor":null,"cost":{"elapsed_ns":91375,"segments_examined":12,"segments_pruned":11}}
```

| Field | Type | Description |
|---|---|---|
| `spans` | array | The matching spans, in Traza's stable order (or `sort` order) |
| `next_cursor` | string or null | Pass as `cursor=` for the next page; `null` at the end |
| `cost.elapsed_ns` | integer | Time inside the engine, excluding serialization |
| `cost.segments_examined` | integer | Segments the query considered |
| `cost.segments_pruned` | integer | Segments skipped whole because their timestamp range could not hold a match |

`cost` is counted per query rather than sampled from the process-wide
counters, which race under concurrent readers and cannot be attributed. It is
what lets a caller — or the dashboard — state how much of the store a window
actually eliminated instead of asserting that filtering is cheap.

An empty result is `{"spans":[],...}`, not a `404`.

**Errors.** `400` for an unknown parameter or an unparseable numeric value
(`{"error":"invalid limit"}`, `{"error":"invalid min_duration_ms"}`,
`{"error":"invalid since"}`, `{"error":"invalid until"}`). `503` on store
failure.

### `GET /v1/traces/{trace_id}`

Every span carrying `trace_id`, ordered by start time, plus every annotation
attached to that trace. The id is percent-decoded from the path. Accepts
`?tenant=`: an empty value names the default tenant explicitly, omitting it
searches every tenant; a bound credential is forced onto its own, and naming
a foreign tenant is a `403`.

```sh
curl http://localhost:8080/v1/traces/trace-1
```

```json
{"annotations":[],"spans":[{"attributes":{"http.method":"POST","region":"us-east"},"end_time_ns":1700000000002500000,"events":[],"name":"charge","parent_span_id":null,"service":"checkout","span_id":"span-1","start_time_ns":1700000000000000000,"status":"ok","trace_id":"trace-1"}],"trace_id":"trace-1"}
```

| Field | Type | Description |
|---|---|---|
| `trace_id` | string | The requested id |
| `spans` | array | The trace's spans, ordered by start time |
| `annotations` | array | Annotations on the trace or any of its spans |

**Errors.** `404 {"error":"trace not found"}` when no span carries the id.
`400 {"error":"query deadline exceeded: ..."}` when the lookup exhausts
[`--query-deadline-ms`](../configuration.md#server-flags). `503` on store
failure.

### `GET /v1/export`

Streams matching spans as chunked NDJSON — one span per line — with bounded
memory. An export larger than RAM is fine.

**Parameters.** Identical to [`GET /v1/spans`](#get-v1spans), with one
difference: **exports are unbounded by default.** Passing `limit` explicitly
caps the stream. Unbounded includes the compute budget: an export is exempt
from `--query-deadline-ms`, because a stream that already said `200` must
not be truncated by a budget mid-flight — completeness stays whatever the
trailers say, and the socket write timeout is what bounds an abandoned
consumer.

**Response `200`.**

```
Content-Type: application/x-ndjson
Transfer-Encoding: chunked
Trailer: X-Traza-Export-Complete, X-Traza-Export-Count
Connection: close
```

The body is one JSON span per line. The stream ends with HTTP trailers:

```
X-Traza-Export-Complete: true
X-Traza-Export-Count: 1
```

**Programmatic clients must check `X-Traza-Export-Complete`.** A `false` value
means the stream failed *after* its `200` response had begun — the only way to
signal that, since the status line is long gone. `X-Traza-Export-Count` is the
number of rows actually emitted and can be cross-checked.

**An export is a snapshot.** The store is pinned when the export starts, and
every page comes from that one state. Spans ingested, replaced, or expired
while the stream is running do not appear and do not change what does: the
output is exactly the dataset that existed at the first byte, with each
primary key appearing at most once. `complete: true` therefore means "this is
that whole dataset", not merely "the connection ended tidily".

A pinned export holds the segment files it is reading, so compaction and
retention cannot reclaim their disk space until it finishes. Exporting a large
store over a slow connection delays reclamation for as long as it runs.

This route always closes its connection.

```sh
curl 'http://localhost:8080/v1/export?service=support-agent' > dataset.ndjson
```

### `GET /v1/tail`

Streams spans as they are admitted, as [server-sent events](https://html.spec.whatwg.org/multipage/server-sent-events.html).

**This is the only route ordered by admission rather than event time**, and
that is the reason it exists. `GET /v1/spans?since=` answers "what *started*
after time T", which cannot express "what is arriving": a span that runs longer
than the client's polling interval starts before the watermark and arrives
after it, so a polling tail drops it permanently. Not late — never. The tail
assigns each admission a sequence number instead, so a span is delivered when
it lands regardless of when it started.

**Parameters.** Every predicate [`GET /v1/spans`](#get-v1spans) accepts, except
the event-time window, plus:

| Parameter | Type | Description |
|---|---|---|
| `cursor` | string | Resume position from a previous frame. Omit to start fresh |
| `backfill` | integer | Spans of history to open with. Default **200**, `0` for future-only. Ignored when `cursor` is given |
| `limit` | integer | Maximum spans per frame. Default **200** |

`since`, `since_ns`, `until` and `until_ns` are **rejected** with `400` rather
than ignored — a tail cannot honour an event-time window, and silently dropping
one would answer a different question than the one asked.

**Response `200`.**

```
Content-Type: text/event-stream
Transfer-Encoding: chunked
Connection: close
```

Three frame types:

```
event: spans
data: {"spans":[{...}],"cursor":"1763913600000000000.41"}

event: gap
data: {"missed":20}

: tick
```

A `spans` frame carries matches in admission order and the position to resume
from. Its `spans` array may be empty while `cursor` still advances — that means
spans were admitted but none matched the filter, and the position moves so a
narrow subscriber does not fall off the back of the ring.

A **`gap`** frame means the subscriber fell further behind than the server
retains. **It carries no position, deliberately.** The dropped entries are
precisely the ones no longer addressable, and `/v1/spans` is ordered by event
time, so it cannot name an admission range at all — there is no query that
fetches "what was dropped". `missed` is how many admissions were lost, or
`null` when the cursor came from a previous process and the count would not be
comparable.

A gap is a **discontinuity**. The server restarts the subscription at the live
edge and the next `spans` frame is a fresh backlog; the client discards what it
was holding and rebuilds from that frame. One ordered source, no overlap, and
no claim of completeness across the break. Spans lost to a gap are still in the
store — they are reachable through [`GET /v1/spans`](#get-v1spans), just not as
part of this stream.

A line beginning `:` is a comment — the heartbeat, sent every 15 seconds on a
quiet store. It is how an intermediary is kept from reaping the connection and
how the server discovers a client has gone.

**A gap does not change `backfill`.** After a gap the stream resumes with the
backlog you asked for — including none, if you asked for `backfill=0`. The
server does not substitute a default.

**A streamed span has been acknowledged.** Entries reach the stream only after
the ingest that carried them succeeded — after the write-ahead log's fsync, and
after the seal that `--durability flushed` promises. The tail is bounded and may
gap, but it never shows a span the store did not accept, and never one that a
crash a moment later would erase.

**Cursors do not survive a restart.** The `epoch` half of the token identifies
the process; a cursor from a previous one is reported as a gap rather than
misread as a live position.

**Retention is bounded and in memory.** The server keeps recent admissions
under two bounds, whichever binds first: `--tail-ring-spans` (default **8192**)
and `--tail-ring-bytes` (default **32 MiB**). Beyond either, a subscriber gaps.
The byte bound is the one that actually caps memory — the ring owns whole spans,
and an LLM span carrying a prompt is orders of magnitude larger than one
carrying a status code. This costs no disk and adds no field to the stored span.
Current residency and both bounds are reported at
[`GET /v1/metrics.json`](#get-v1metricsjson) under `tail_ring`.

This route always closes its connection, and is counted but never timed in
[route-class metrics](../operations/monitoring.md).

```sh
curl -N 'http://localhost:8080/v1/tail?service=support-agent&backfill=0'
```

---

## Sessions and analytics

### `GET /v1/sessions`

Sessions active in the window, most recent activity first.

**Session identity is `(tenant, session_id)`**: two tenants reusing an id are
two sessions, never one merged row — the primary key doing for sessions what
it does for spans.

| Parameter | Type | Description |
|---|---|---|
| `since` / `since_ns` | integer | Window start, Unix nanoseconds |
| `until` / `until_ns` | integer | Window end, Unix nanoseconds |
| `limit` | integer | Maximum sessions returned. Default **100** |
| `tenant` | string | Only this tenant's sessions. Empty names the default tenant explicitly; omitted lists every tenant's (the operator view, each row carrying its tenant) |

`group_by` is explicitly rejected here: `400 {"error":"group_by is not a
/v1/sessions parameter"}`.

**Response `200`.**

```json
{"sessions":[{"completion_tokens":88,"cost_usd":0.0031,"error_count":0,"first_start_ns":1700000001000000000,"last_end_ns":1700000001900000000,"llm_calls":1,"prompt_tokens":412,"session_attribute":"gen_ai.conversation.id","session_id":"chat-4711","span_count":1,"total_tokens":500,"trace_count":1}]}
```

| Field | Type | Description |
|---|---|---|
| `session_id` | string | The identifier shared by the session's spans |
| `tenant` | string | Whose spans formed the session; **omitted for the default tenant** |
| `session_attribute` | string | Which recognized key grouped this session |
| `first_start_ns` / `last_end_ns` | integer | Activity window |
| `trace_count` | integer | Distinct traces containing session spans |
| `span_count` | integer | Spans carrying this session id |
| `llm_calls` | integer | Spans recognized as LLM calls |
| `prompt_tokens` / `completion_tokens` / `total_tokens` | integer | Token sums |
| `cost_usd` | number | Summed cost, when ingest supplied one |
| `error_count` | integer | Spans with status `error` |

**Errors.** `400` for an unknown parameter or unparseable number. `503` on
store failure.

### `GET /v1/sessions/{id}`

One session's rollup plus its per-trace breakdown. The id is percent-decoded
and resolved under any recognized session key. Accepts `?tenant=` with the
same semantics as [`GET /v1/traces/{trace_id}`](#get-v1tracestrace_id) —
session identity is `(tenant, session_id)`, so the same id under two tenants
is two different sessions.

**Response `200`.** Every field of a session summary, at the top level, plus
`traces`:

```json
{"completion_tokens":88,"cost_usd":0.0031,"error_count":0,"first_start_ns":1700000001000000000,"last_end_ns":1700000001900000000,"llm_calls":1,"prompt_tokens":412,"session_attribute":"gen_ai.conversation.id","session_id":"chat-4711","span_count":1,"total_tokens":500,"trace_count":1,"traces":[{"cost_usd":0.0031,"error_count":0,"first_start_ns":1700000001000000000,"last_end_ns":1700000001900000000,"root_name":"openai.chat","span_count":1,"total_tokens":500,"trace_id":"trace-2"}]}
```

Each entry of `traces` carries `trace_id`, `root_name` (the name of the
trace's earliest session span), `first_start_ns`, `last_end_ns`, `span_count`,
`total_tokens`, `cost_usd`, and `error_count`, ordered by first activity.

**Errors.** `404 {"error":"session not found"}`. `503` on store failure.

### `GET /v1/stats/llm`

Token and cost aggregation, sorted by cost, then tokens, then key.

| Parameter | Type | Description |
|---|---|---|
| `group_by` | enum | `model`, `provider`, `service`, `session`, or `day`. Default `model` |
| `since` / `since_ns` | integer | Window start, Unix nanoseconds |
| `until` / `until_ns` | integer | Window end, Unix nanoseconds |
| `limit` | integer | Truncates the row list. Unbounded by default |
| `tenant` | string | Only this tenant's spans. Empty names the default tenant explicitly; omitted aggregates every tenant |

With `group_by=session`, rows merge **structurally** by `(tenant, session_id)`
and only then render a display key: a non-default tenant's row renders `key`
as `tenant/session_id`. Merging by the rendered string instead would fuse a
default-tenant session literally named `acme/chat` with tenant `acme`'s
session `chat` — two rows may still *render* alike, but merged counters would
be wrong where equal text is merely honest.

**Response `200`.**

```json
{"rows":[{"completion_tokens":88,"cost_usd":0.0031,"error_count":0,"key":"gpt-4o","llm_calls":1,"llm_duration_ns":900000000,"prompt_tokens":412,"spans":1,"total_tokens":500}]}
```

| Field | Type | Description |
|---|---|---|
| `key` | string | Model name, provider, service, session id, or `YYYY-MM-DD` UTC day |
| `spans` | integer | All spans in the group |
| `llm_calls` | integer | Spans recognized as LLM calls |
| `prompt_tokens` / `completion_tokens` / `total_tokens` | integer | Token sums |
| `cost_usd` | number | Summed cost |
| `error_count` | integer | Spans with status `error` |
| `llm_duration_ns` | integer | Summed LLM-call duration; divide by `llm_calls` for average latency |

Aggregates are exact. Sealed segments contribute cached rollups — segments are
immutable, so a rollup is computed once — and window edges fall back to
decoding only the boundary segments. Integer counters saturate at `u64::MAX`
rather than wrapping; non-finite cost values are ignored.

**Errors.** `400 {"error":"group_by must be model|provider|service|session|day"}`
for an unrecognized dimension; `400` for an unknown parameter. `503` on store
failure.

**The aggregation routes below do not block ingest.** Each folds the match set
in one pass and constant memory, against a snapshot taken up front — one copy
of the bounded write buffer and one reference per segment — so a scan of the
whole corpus runs alongside writes rather than stopping them, and reports one
coherent instant rather than a moving one.

### `GET /v1/stats/series`

Buckets matching spans into even time buckets. Accepts every
[`GET /v1/spans`](#get-v1spans) filter, plus:

| Parameter | Type | Description |
|---|---|---|
| `since` / `until` | integer | **Required.** The window, Unix nanoseconds |
| `buckets` | integer | Bucket count, clamped to 1–512. Default 24 |

One pass produces volume, errors, LLM calls, tokens, cost and duration
percentiles together, because every surface that wants one of them wants
several, and separate routes would mean separate scans of the same window.

```sh
curl 'http://localhost:8080/v1/stats/series?since=1700000000000000000&until=1700086400000000000&buckets=24&service=checkout'
```

```json
{"since_ns":1700000000000000000,"until_ns":1700086400000000000,"bucket_ns":3600000000000,"buckets":[{"start_ns":1700000000000000000,"spans":412,"errors":7,"llm_calls":180,"total_tokens":220144,"cost_usd":0.7628,"p50_ns":1811939327,"p95_ns":7784628223}]}
```

`buckets` is always exactly the requested length, so a quiet period is a
visible gap rather than something the caller reconstructs from timestamps.

**Errors.** `400 {"error":"since and until are required for a series"}`;
`400 {"error":"until must be after since"}`; `400` for an unknown parameter.

### `GET /v1/stats/duration`

The duration distribution of matching spans. Accepts every
[`GET /v1/spans`](#get-v1spans) filter.

```sh
curl 'http://localhost:8080/v1/stats/duration?service=checkout&since=1700000000000000000'
```

```json
{"count":1852,"min_ns":1000000,"max_ns":118000000000,"mean_ns":3402118942,"p50_ns":2550136831,"p75_ns":4160749567,"p90_ns":7516192767,"p95_ns":11274289151,"p99_ns":51539607551,"buckets":[{"upper_ns":1048575,"count":3}]}
```

Percentiles are the upper bound of a log-linear bucket: **at most 6.25% high,
never low.** `min_ns`, `max_ns`, `mean_ns` and `count` are exact. Only occupied
buckets are returned — a distribution spanning nanoseconds to minutes fills a
few dozen of the thousand, and the zeros would be almost the whole payload.

### `GET /v1/stats/failures`

Error spans grouped by `(service, name, status)`, most frequent first. Accepts
every [`GET /v1/spans`](#get-v1spans) filter; `limit` caps the group count
(default 100). Without an explicit `status`, the filter defaults to
`status=error`.

```sh
curl 'http://localhost:8080/v1/stats/failures?since=1700000000000000000&limit=20'
```

```json
{"groups":[{"service":"checkout-api","name":"tool.refund_lookup","status":"error","count":142,"first_seen_ns":1700000000000000000,"last_seen_ns":1700003600000000000,"example_trace_id":"trace-9f2c","example_span_id":"span-a","p50_ns":325000000,"p95_ns":812000000}],"total":169,"distinct":7,"groups_omitted":0,"spans_untracked":0}
```

| Field | Type | Description |
|---|---|---|
| `groups` | array | Signatures, most frequent first, truncated to `limit` |
| `total` | integer | **Every** matching span, counted before truncation |
| `distinct` | integer | Signatures seen, up to the cardinality bound |
| `groups_omitted` | integer | Signatures measured but cut by `limit` |
| `spans_untracked` | integer | Spans whose signature was never tracked because the cardinality bound was reached |

**Use `total` as the denominator for a share, not the sum of `groups`.** The
returned page is truncated, so summing it overstates every signature's
fraction of the whole — by more, the more signatures exist.

**Grouping is bounded at 4,096 distinct signatures.** Each tracked signature
costs a duration histogram, so an unbounded map turns high-cardinality error
text — an id or a timestamp in a span name — into gigabytes of server memory
to answer a question whose useful form is twenty rows. Past the bound, spans
are counted into `total` but not grouped, and `spans_untracked` says how many.
A non-zero value means `distinct` is a floor.

Grouping happens server-side because the input can be every error in the
window while the useful answer is a dozen rows: shipping the spans so a client
could group them would move megabytes to fill one screen. `example_trace_id`
is the most recent occurrence, so a group is something you can open.

### `GET /v1/stats/slowest`

The slowest matching spans, ranked across the whole match set rather than
within an arbitrary first page. Accepts every
[`GET /v1/spans`](#get-v1spans) filter; `limit` defaults to 10.

```sh
curl 'http://localhost:8080/v1/stats/slowest?since=1700000000000000000&limit=10'
```

```json
{"spans":[{"trace_id":"trace-1","span_id":"span-1","name":"charge","service":"checkout","start_time_ns":1700000000000000000,"end_time_ns":1700000118000000000,"status":"ok","attributes":{},"events":[]}]}
```

Unlike `sort=-duration` on [`GET /v1/spans`](#get-v1spans), this route has no
candidate ceiling: it keeps only `limit` spans while folding, so memory is
bounded by the answer rather than by the match set. `limit` is itself capped at
1,000 — a tail is read, not paged, and a larger request is a way to ask the
server to hold the match set in memory.

---

## Runs: tapes, replays, diffs and recall

A **run** is a session id, or `trace:<trace_id>` for one trace
([run identity](../llm-semantics.md#run-identity)); its **steps** are its
`llm`, `tool` and `agent` spans in `(start, trace, span)` order. These routes
read runs that way. The [replay guide](replay.md) walks through them with a
worked example; this section is the reference.

The tape, diff and recall routes take the usual `tenant=` selector and
binding, and `POST /v1/replays` takes the same as a `tenant` body field: a
bound credential reads its own tenant, and naming another is `403`. A run that
has no spans under the caller's scope is `404 {"error":"run not found"}`. The
routes under `/v1/replays/{id}` take **no query parameters** — an SDK sends
none, and a replay's tenant was fixed when it was created — and ignore any
that are sent.

**Replay routes are reads under any method.** Creating a replay snapshots the
store and changes nothing in it, and the provider-shaped routes must be `POST`s
because that is what an SDK sends — so every route under `/v1/replays` accepts
any valid credential, `ro` included. And on the routes under `/v1/replays/`
only, `x-api-key: TOKEN` (the Anthropic SDK's header) is accepted as the
credential when `Authorization` is absent.

### `GET /v1/tapes/{run}`

The run's tape.

| Parameter | Type | Description |
|---|---|---|
| `tenant` | string | Tenant scope, as on every route |
| `resolve` | `true` / `false` | Inline `$payload` references in requests and responses, up to `--tape-max-bytes` of payload content per tape, within the request's one `--query-deadline-ms` budget (past it, `400`). Default `false` |
| `max_steps` | integer | Steps to return. Default and maximum 20,000 |

```sh
curl 'http://localhost:8080/v1/tapes/weather-7?max_steps=1'
```

```json
{"run":"weather-7","spans_examined":4,"steps":[{"duration_ns":390000000,"fingerprint":"llm:gpt-4o-2024-08-06#8b9a07d4aac0a133","index":0,"kind":"llm","model":"gpt-4o-2024-08-06","name":"openai.chat","parent_span_id":"wf","request":[{"content":"You are a weather assistant.","role":"system"},{"content":"What's the weather in Paris?","role":"user"}],"request_digest":"8b9a07d4aac0a133c789406dfccd9477618c1049ca0255f58b262fc72fca3d8b","response":[{"finish_reason":"tool_calls","role":"assistant","tool_calls":[{"arguments":{"city":"Paris"},"id":"call_w1","name":"get_weather"}]}],"response_digest":"bc0a9a93dfc45c687262d0e46fceb83a1c7c92d362ccb045210310872c51d35f","service":"weather-agent","span_id":"llm-1","start_ns":1791964800010000000,"status":"ok","tokens":{"cache_read":null,"cache_write":null,"input":82,"output":17},"trace_id":"4bf92f3577b34da6a3ce929d0e0e4736"}],"tenant":null,"truncated":true}
```

| Field | Type | Description |
|---|---|---|
| `run` | string | The run id asked for |
| `tenant` | string or null | The tenant scope read under; `null` is the operator view across tenants |
| `steps[].index` | integer | Position on the tape, from 0 |
| `steps[].kind` | string | `llm`, `tool` or `agent` |
| `steps[].name`, `service`, `status` | string | The span's own fields |
| `steps[].model`, `tool`, `agent` | string | Present when the span names one |
| `steps[].start_ns`, `duration_ns` | integer | Start time and `end - start` |
| `steps[].request`, `response` | any | The step's input and output as stored ([precedence](../llm-semantics.md#input-and-output)); omitted when not recorded |
| `steps[].request_digest`, `response_digest` | string | SHA-256 of the **stored** value's canonical JSON, or the content address of a `$payload` reference |
| `steps[].fingerprint` | string | `kind:name#` plus 16 hex characters of the request digest |
| `steps[].tokens` | object | `input`, `output`, `cache_read`, `cache_write`; each `null` when not reported |
| `steps[].cost_usd` | number | Metered, or priced from `--pricing`; omitted when neither |
| `truncated` | boolean | The run had more than 20,000 spans, or more steps than `max_steps` |
| `spans_examined` | integer | Spans read, steps and non-steps alike |
| `resolved_partial` | boolean | Only with `resolve=true`: some reference stayed a reference — the byte budget ran out (the cut is a prefix: everything after the first reference that did not fit stays unresolved), or its bytes are gone |

### `POST /v1/replays`

Snapshots a run's tape into a replay that serves its model answers back.

```sh
curl -X POST http://localhost:8080/v1/replays -d '{"run":"weather-7","mode":"loose"}'
```

```json
{"base_urls":{"anthropic":"/v1/replays/rp_70235b53074a150e1cca9771c76a88bf/anthropic","openai":"/v1/replays/rp_70235b53074a150e1cca9771c76a88bf/openai/v1"},"expires_at_ns":1792065849919268000,"id":"rp_70235b53074a150e1cca9771c76a88bf","llm_steps":2,"mode":"loose","run":"weather-7","steps":3,"unreadable_tool_steps":[]}
```

| Body field | Type | Description |
|---|---|---|
| `run` | string, required | The run to replay |
| `tenant` | string | Tenant scope; a bound credential's own tenant by default |
| `mode` | `strict` / `loose` | `strict` (default) refuses a request whose messages do not match the recorded step; `loose` serves in order regardless. See [matching](#post-v1replaysidopenaiv1chatcompletions) |

`201` on success. The replay lives in memory for one hour from creation and
does not survive a restart. A replay created under a tenant is usable by that
tenant's credentials and unbound operators; one created from the operator view
(no tenant) by unbound operators only. It holds each model step's answer and
each tool step's arguments and result, resolved within `--tape-max-bytes`;
recorded requests are resolved one at a time, digested for strict matching,
and not held. `unreadable_tool_steps` lists the tape indices of tool steps
whose value could not be resolved, or that recorded no result at all (see
[`tools/next`](#post-v1replaysidtoolsnext)).

| Response | Cause |
|---|---|
| `400 {"error":"…"}` | Missing `run`, unknown field, or a bad `mode` / `tenant` |
| `403` | A bound credential named another tenant |
| `404 {"error":"run not found"}` | No spans carry that run id in scope |
| `409 {"error":"step 0 (llm:gpt-4o) recorded no response — …"}` | The run cannot be replayed: a model step with no recorded answer (content capture off); an offloaded answer that cannot be read, or that is past `--tape-max-bytes` (the message says which); in strict mode a request that cannot be read or was recorded as text rather than a message list; a run of more than 20,000 spans; or a snapshot larger than one tenant's whole share (64 MiB), which can never be held — refused as soon as building it passes the share. The message names the step and the remedy. Carries `x-should-retry: false` |
| `429` with `Retry-After` | No room now. Each tenant's data — and the operator view, as a share of its own — may be held by 64 live replays and 64 MiB of memory; the registry as a whole by 1,024 replays and 512 MiB. Memory is everything a replay retains, counted generously: answers, tool records, identifiers, erasure bookkeeping, collection overhead, and its call log at its 4,096-entry bound (about 96 KiB). A full share answers with the seconds until enough of that share's replays expire. A full registry answers the same to unbound operators and a flat `60` to bound credentials, which do not learn when another tenant's replay expires. A live replay is never evicted |

### `GET /v1/replays/{id}`

What a replay has served.

```json
{"calls":[{"at_ns":1792062255964194000,"matched":true,"step":0}],"diverged":null,"expires_at_ns":1792065849919268000,"id":"rp_70235b53074a150e1cca9771c76a88bf","mode":"loose","remaining":1,"run":"weather-7","served":1,"unreadable_tool_steps":[]}
```

`served` and `remaining` count model steps. `calls` lists each model call
served or refused for divergence — `step` is its tape index and `matched`
whether its messages matched the recording (informational in loose mode) — up
to 4,096 entries. `diverged` is the first divergence, `{"step",
"expected_digest", "got_digest"}`, or `null`. An unknown, expired or foreign
replay is `404 {"error":"replay not found"}`. A replay withdrawn by an
erasure is `410 {"error":"…","type":"replay_erased","erasure":N}` on this and
every route under it, from the moment the erasure is recorded until the
replay would have expired. A withdrawn replay takes no live replay's room;
each share keeps at most 64 withdrawn replays (the registry 1,024), and past
that forgets the one closest to expiring, whose id is then `404`.

### `POST /v1/replays/{id}/openai/v1/chat/completions`

The OpenAI SDK's chat-completions call, answered with the next recorded model
step: set the SDK's base URL to the replay's `base_urls.openai`. The body is an
ordinary chat-completions request. The answer is a `chat.completion` object —
the recorded text, the recorded tool calls as `tool_calls` (arguments as a JSON
string), the recorded finish reason translated into OpenAI's vocabulary, the
recorded model and token usage. With no recorded model the request's `model` is
echoed, or `"unknown"`; a model name or response id over 256 bytes, recorded or
requested, counts as absent. An answer recorded as text is served as that text
even when it reads as JSON. No response, streamed or not, exceeds 64 MiB. With
`"stream": true` it is `text/event-stream`: `chat.completion.chunk` events, a usage chunk when
`stream_options.include_usage` is set, then `data: [DONE]`. The stream is
reconstructed from stored text in Traza's own chunking.

**Matching.** Model steps are served in recorded order. In `strict` mode the
request's `messages` are normalised into the message shape step facts use and
digested; the recorded request is normalised the same way (for the indexed
layouts and native events its digest is the tape's `request_digest`). Only the
messages are compared — not the model or other parameters.

| Response | Cause |
|---|---|
| `200` | The next recorded answer; the step is consumed |
| `400 {"error":{"type":"invalid_request_error",…}}` | The body is not a JSON object with a `messages` array; or a path other than `chat/completions` under the base URL |
| `404 {"error":{"type":"not_found_error",…}}` | Unknown, expired or foreign replay |
| `409 {"error":{"type":"replay_divergence","message","step","expected_digest","got_digest"}}` | Strict mode: the messages do not match recorded step `step`. The step is **not** consumed. Carries `x-should-retry: false` |
| `409 {"error":{"type":"response_too_large","message","step","limit"}}` | The next recorded answer would render past 64 MiB in the asked-for shape — a stream can be many times its recording. The step is **not** consumed; the same step unstreamed may fit. Carries `x-should-retry: false` |
| `410 {"error":{"type":"replay_exhausted","message","served"}}` | Every recorded model step has been served. Carries `x-should-retry: false` |
| `410 {"error":{"type":"replay_erased","message","erasure"}}` | An erasure recorded after the replay was created covers content it held; the replay is withdrawn. Carries `x-should-retry: false` |

### `POST /v1/replays/{id}/anthropic/v1/messages`

The Anthropic SDK's messages call, by the same rules: set the SDK's base URL
to the replay's `base_urls.anthropic`. A `system` prompt is normalised as a
leading system message. The answer is a `message` object — a `text` block for
the recorded text and a `tool_use` block per recorded tool call, the recorded
stop reason translated into Anthropic's vocabulary, and usage including
`cache_read_input_tokens` / `cache_creation_input_tokens` when recorded. A
recording holding several assistant messages is served as one message. With
`"stream": true`: `message_start`, `content_block_start` / `content_block_delta`
(`text_delta`, `input_json_delta`) / `content_block_stop` per block,
`message_delta`, `message_stop`.

```sh
curl -X POST http://localhost:8080/v1/replays/rp_70235b53074a150e1cca9771c76a88bf/anthropic/v1/messages \
  -H 'x-api-key: TOKEN' -H 'anthropic-version: 2023-06-01' \
  -d '{"model":"claude-x","max_tokens":256,"system":"You are a weather assistant.","messages":[{"role":"user","content":"What'"'"'s the weather in Paris?"}]}'
```

```json
{"content":[{"id":"call_w1","input":{"city":"Paris"},"name":"get_weather","type":"tool_use"}],"id":"msg_traza70235b53074as0","model":"gpt-4o-2024-08-06","role":"assistant","stop_reason":"tool_use","stop_sequence":null,"type":"message","usage":{"input_tokens":82,"output_tokens":17}}
```

Errors take Anthropic's shape, `{"type":"error","error":{"type",…}}`, with the
same statuses and types as the OpenAI route:

```json
{"error":{"message":"all 2 recorded model calls have been served","served":2,"type":"replay_exhausted"},"type":"error"}
```

### `POST /v1/replays/{id}/tools/next`

The next recorded tool step not yet served, on a cursor independent of the
model steps — for a harness that replays tool results instead of running the
tools. Body `{"tool": "<name>"}` takes the next one with that name; an empty
body or `{}` takes the next of any name.

```json
{"arguments":{"city":"Paris"},"result":{"celsius":12,"forecast":"rain"},"step":1,"tool":"get_weather"}
```

Errors on this route, and on `GET /v1/replays/{id}`, take the API's own
shape — `{"error": "<message>"}` with the machine-readable kind beside it as
`type` — rather than a provider's:

| Response | Cause |
|---|---|
| `400 {"error":"…"}` | A body that is not `{}` or `{"tool": "<name>"}` |
| `404 {"error":"replay not found"}` | Unknown, expired or foreign replay |
| `409 {"error":"…","type":"tool_unreadable","step"}` | The next tool step's arguments or result was offloaded and could not be resolved — erased, swept by retention, or past `--tape-max-bytes` — or it recorded no result at all (content capture off). A result recorded as `""` or `null` is served as recorded. The step is **not** consumed. Carries `x-should-retry: false` |
| `410 {"error":"…","type":"replay_exhausted"}` | No unserved tool step (of that name) remains. Carries `x-should-retry: false` |
| `410 {"error":"…","type":"replay_erased","erasure"}` | The replay was withdrawn by an erasure. Carries `x-should-retry: false` |

### `GET /v1/diff`

Two runs aligned step by step. Parameters: `a` and `b` (required run ids),
`tenant`.

```sh
curl 'http://localhost:8080/v1/diff?a=weather-7&b=weather-8'
```

```json
{"a":{"cost_usd":0.0,"duration_ns":1000000000,"errors":0,"outcome":"success","run":"weather-7","steps":3,"tokens":233,"truncated":false},"aligned":[{"a":0,"b":0,"fingerprint_a":"llm:gpt-4o-2024-08-06#8b9a07d4aac0a133","fingerprint_b":"llm:gpt-4o-2024-08-06#1ae84ec741143bc4","op":"changed","why":"input"},{"a":1,"b":1,"fingerprint_a":"tool:get_weather#6e1e312d537bc71b","fingerprint_b":"tool:get_weather#ac8cc520a3295a36","op":"changed","why":"input"},{"a":2,"b":2,"fingerprint_a":"llm:gpt-4o-2024-08-06#6db28425e9e18cdb","fingerprint_b":"llm:gpt-4o-2024-08-06#592c7b1d345f6104","op":"changed","why":"input"}],"b":{"cost_usd":0.0,"duration_ns":1000000000,"errors":0,"outcome":"success","run":"weather-8","steps":3,"tokens":233,"truncated":false},"delta":{"cost_usd":0.0,"duration_ns":0,"errors":0,"steps":0,"tokens":0},"first_divergence":{"a":0,"b":0,"explanation":"step 0 of a and step 0 of b (llm:gpt-4o-2024-08-06) received different input: request 8b9a07d4aac0 in a, 1ae84ec74114 in b","kind":"input"},"truncated":false}
```

| Field | Type | Description |
|---|---|---|
| `a`, `b` | object | Per run: `run`, `steps`, `cost_usd` and `tokens` over every span (cost metered or priced), `duration_ns` (latest end minus earliest start), `errors` (spans with status `error`), `outcome` (`success`, `failure`, `abandoned`, `unknown`), and `truncated`: the run had more than 50,000 spans, so every figure describes its first 50,000 and its outcome is `unknown` |
| `aligned[]` | array | `op` (`same`, `changed`, `insert`, `delete`), the step index on each side (`null` on the side without it), both fingerprints, and `why` (`input`, `decision`, `config`, `structure`) unless `same` |
| `first_divergence` | object or null | The first entry that is not `same`: `a`, `b`, `kind`, `explanation`. For an insertion or deletion the side without the step reports the index of its next step |
| `delta` | object | `b` minus `a`: `cost_usd`, `tokens`, `duration_ns`, `errors`, `steps` |
| `truncated` | boolean | A side had more than 5,000 steps or 50,000 spans; the alignment describes prefixes |

Alignment is a longest common subsequence over `kind:name` signatures. Equal
signature, input, output and configuration is `same`; a different input is
`input`; the same input with a different output is `decision`; a model call
whose `gen_ai.request.*` parameters differ is `config`, and so is a model swap
— two model calls naming different models in the same unaligned stretch are
paired as one `config` change. A step only one run has is `structure`. Step
status is not compared. `404 {"error":"run not found: <run>"}` names a run
with no spans; `400` when `a` or `b` is missing, or when reading, extracting
and aligning the two runs does not fit the request's one `--query-deadline-ms`
budget — the alignment compares interned signatures and checks the budget as
it goes, so long names cannot keep it running past it.

### `GET /v1/recall`

Earlier runs whose recorded text contains every word of `q`.

| Parameter | Type | Description |
|---|---|---|
| `q` | string, required | Words every matching span must contain — whole words, any order, exactly as [`content=`](#content-search-content) |
| `outcome` | `success` / `failure` / `any` | Only runs that ended this way. Default `any`, which ranks successes first |
| `limit` | integer | Runs to return. Default 10, capped at 50 |
| `tenant` | string | Tenant scope |

```sh
curl 'http://localhost:8080/v1/recall?q=weather+Paris&limit=1'
```

```json
{"runs":[{"cost_usd":0.0,"last_end_ns":1791964801000000000,"outcome":"success","outcome_source":"derived","run":"weather-7","score":3,"skeleton":["llm:gpt-4o-2024-08-06","tool:get_weather","llm:gpt-4o-2024-08-06"],"skeleton_truncated":false,"steps":3,"tenant":"","truncated":false}],"truncated":false}
```

Ranking: outcome first (successes first under `any`), then `score` — the
number of the run's spans that matched — then the latest `last_end_ns`. Each
run carries `outcome_source` (`declared` or `derived`), `goal` when a span
declared one, `steps`, `cost_usd`, and a `skeleton`: step signatures in order
with consecutive repeats folded as `"tool:search x3"`, at most 40 entries
(`skeleton_truncated` says when more were cut). A failed run adds `cause`,
`{"trace_id","span_id","name","rule"}`, the step its failure is attributed to.
`tenant` disambiguates a run id two tenants share in the operator view. A run
whose last span ended within the last 15 minutes is `unknown`. Each run is
read up to its first 20,000 spans; a longer one says `truncated: true`, its
`steps`, `cost_usd`, `last_end_ns` and skeleton describe those spans, and its
outcome is `unknown` with no `cause` — how a run ended cannot be read from its
beginning, and ranking it by its beginning could rank a failure as a success.

Recall groups the newest 5,000 matching spans into runs and reads the 100 runs
with the most matches among them; `truncated: true` says the first bound
applied. A `q` with no ASCII letters or digits is `400` — it could match
nothing — and so is a `q` whose words appear in more than 200,000 spans, too
many to rank by recency; add a word.

---

## Annotations

### `POST /v1/annotations`

Records one annotation against a **typed subject**, without mutating any
span. The subject is expressed by which address fields are set, and exactly
one of four shapes must hold:

| Subject | Address fields | Meaning |
|---|---|---|
| **trace** | `trace_id` alone | Judgment about the trace as a whole |
| **span** | `trace_id` + `span_id` | Judgment about one span |
| **session** | `session_id` alone | Judgment about a session as a whole |
| **experiment example** | `experiment_id` + `example_id` | A **score**. Optionally also `trace_id`/`span_id`, naming the task run's span — which is what makes a score address the `(experiment, example, span)` tuple |

**Body.**

| Field | Type | Required | Description |
|---|---|---|---|
| `trace_id` | string | by subject | Trace containing the annotated span; on a score, the run's trace |
| `span_id` | string | no | Annotated span; requires its `trace_id` |
| `tenant` | string | no | The annotation's tenant; empty is the default tenant. Scoped exactly like span identity — reads filter on it, erasure dooms by it. `$tenant` is accepted as an alias so the span's spelling routes correctly. A bound credential stamps its binding onto an empty value and answers `400` when a different one is named |
| `session_id` | string | by subject | Session-subject address |
| `experiment_id` | integer | by subject | Experiment half of a score's address |
| `example_id` | string | with `experiment_id` | Example half of a score's address — the stable example id within the experiment's dataset version |
| `name` | string | yes | For example `quality`, `thumbs`, `groundedness` |
| `value` | any JSON | yes | Number, string, boolean, object |
| `source` | string | no | Convention: `human:<who>` or `eval:<evaluator>` |
| `comment` | string | no | Free-form |
| `timestamp_ns` | integer | no | Unix nanoseconds. **Absent or `0` is filled in with the server's current time** |

**A score's address must hold at write time**, validated against the eval
log under its own lock: the experiment must exist (`400
{"error":"no experiment 7"}`), belong to the score's tenant (`400
{"error":"a score's tenant must be its experiment's; experiment 7
disagrees"}`), and list the example in its dataset version's manifest (`400
{"error":"example ex-1 is not in experiment 7's dataset version"}`). Each
refusal names the offender, because a scorer that fires thousands of these
needs to know *which* one was misaddressed.

**Scores are exempt from TTL.** They live on eval retention, not trace
retention: a rolling window that swept January's scores would silently empty
the base of every experiment-over-experiment diff run in March.

```sh
curl -X POST http://localhost:8080/v1/annotations \
  -H 'Content-Type: application/json' \
  -d '{"trace_id":"trace-2","span_id":"span-a","name":"groundedness",
       "value":0.9,"source":"eval:nightly"}'
```

```json
{"recorded":true}
```

While an [erasure](#erasure) covering the addressed subject is pending — the
trace or span, the session, or the annotation's whole tenant — the annotation
is acknowledged and deliberately not stored: the same admission barrier spans
get, so no judgment can attach itself to data mid-erasure and surface after
the settle.

**Errors.** `400` with the parse error for a malformed body; `400` when the
engine rejects the annotation as invalid; `503` on store failure.

### `GET /v1/annotations`

Every supplied narrowing must match. **All of them are optional** — an empty
query returns every annotation, newest first.

| Parameter | Type | Description |
|---|---|---|
| `trace_id` | string | Narrow to one trace |
| `span_id` | string | Narrow to one span |
| `tenant` | string | Narrow to one tenant; empty names the default tenant. Bound credentials are forced onto their own |
| `session_id` | string | Narrow to session-subject annotations for this session id |
| `experiment_id` | integer | Narrow to scores of this experiment |
| `example_id` | string | Narrow to scores of this example |
| `name` | string | Narrow to one annotation name |
| `source` | string | Sources **starting with** this, so `human:` and `eval:` each select a family |
| `since` / `since_ns` | integer | Recorded at or after this timestamp |
| `until` / `until_ns` | integer | Recorded at or before this timestamp |
| `limit` | integer | Maximum returned, applied after ordering |

`trace_id` used to be required, which made an eval run unreadable: its scores
exist per trace, but a run is a population and nobody wants it one trace at a
time. Annotations are indexed in memory at human/eval scale, so the
cross-trace path is a scan of that index rather than anything touching a
segment.

Results are newest first, tie-broken by
`(tenant, trace_id, span_id, example_id, name)` so a bulk import sharing one
timestamp still pages deterministically.

```sh
curl 'http://localhost:8080/v1/annotations?trace_id=trace-2'

# Everything a nightly evaluator scored in the last day
curl 'http://localhost:8080/v1/annotations?source=eval:&since=1700000000000000000&limit=500'
```

```json
{"annotations":[{"comment":"","name":"groundedness","source":"eval:nightly","span_id":"span-a","timestamp_ns":1785005352756011000,"trace_id":"trace-2","value":0.9}]}
```

**Errors.** `400` for an unknown parameter or an unparseable number. `503` on
store failure.

---

## Evals

The eval entity model: datasets → versions (immutable, content-addressed) →
experiments → runs, with **scores riding the annotations surface** (a score
is an annotation addressed to an experiment example — see
[`POST /v1/annotations`](#post-v1annotations)). Identity and addressing only:
no runner, no scorer library, no UI. Task execution stays outside Traza by
design — what these entities close is that telemetry alone cannot *represent*
the eval loop: a failing production trace has nowhere to be promoted to, and
an experiment has no identity to hang scores on.

Everything lives in one append-only JSONL log (`evals.jsonl` at the store
root), fsynced per mutation, a recovery domain exactly like
`annotations.jsonl` and `tombstones.jsonl`.

**Tenancy applies throughout.** Datasets carry a tenant (empty is the default
tenant) and experiments and runs inherit it. A bound credential sees only its
own tenant's entities, and **cross-tenant reads answer `404`, never `403`** —
existence is a fact about another tenant, and a distinguishable refusal would
leak it.

### `POST /v1/datasets`

**Body.** `{"name": "...", "tenant": "..."}` — `tenant` optional; empty is
the default tenant, and a bound credential's binding is stamped or checked
exactly as at span ingest.

```json
{"dataset_id":1}
```

Dataset ids are store-wide, monotonic, and **never reused** — even a tenant
erasure that removes the highest-id datasets leaves a counter record behind,
so an erased id can never be silently re-issued to alias a different dataset
in external references (CI configs, receipts, operator notes).

**Errors.** `400 {"error":"dataset name is empty"}`; `400` for an invalid
tenant; `403` for a bound credential naming a foreign tenant; `409` while an
erasure of the tenant is pending.

### `GET /v1/datasets`

Datasets visible to the caller, each with version summaries. Accepts
`?tenant=` (empty names the default tenant; omitted lists every tenant's, for
operators).

```json
{"datasets":[{"schema":1,"dataset_id":1,"tenant":"acme","name":"support-hard-cases","created_unix_ns":1755200000000000000,"versions":[{"version_id":"63ff05…","examples":12,"created_unix_ns":1755200000000000000,"tombstoned":false}]}]}
```

`tenant` is omitted on default-tenant records; a version summary carries
`parent` only when the version has one.

### `GET /v1/datasets/{id}`

One dataset in the same shape, or `404 {"error":"no such dataset"}` — for an
unknown id and for another tenant's alike.

### `POST /v1/datasets/{id}/versions`

Creates an immutable, content-addressed dataset version.

**Body.**

| Field | Type | Required | Description |
|---|---|---|---|
| `parent` | string | no | The version this one derives from, for lineage. Must be a version of the same dataset, and not tombstoned |
| `provenance` | any JSON | no | What produced this version — a query, an import description. Recorded verbatim; on an idempotent re-POST the *first* write's provenance stands |
| `examples` | array | yes | At least one of `{example_id, input, expected?, split?, provenance?}` |

`example_id` is client-chosen and **stable across versions** — the id is
identity and the digest is content, so a version that re-lists an id with new
content is new lineage for the same logical example. `input` and `expected`
are free JSON and may contain `$payload` references copied from a promoted
span; those count as live references, which is what makes the copy real for
offloaded content. `provenance` on an example names the source
`{tenant?, trace_id?, span_id?}` it was promoted from.

**Response `200`.**

```json
{"version_id":"9c4f…","examples":12,"created":true}
```

`version_id` is the SHA-256 of the canonical manifest
`{dataset_id, parent, examples}`, so **identical content is the identical
version and a re-POST is idempotent by construction** — it answers the same
`version_id` with `"created":false`.

**Errors.**

| Response | Cause |
|---|---|
| `400 {"error":"a version needs at least one example"}` | Empty manifest |
| `400 {"error":"example_id is empty"}` | An example without identity |
| `400 {"error":"example ex-1 appears twice in the manifest"}` | Duplicate example id |
| `400 {"error":"no dataset 7"}` | Unknown dataset — or another tenant's |
| `400 {"error":"parent version 9c4f… is not a version of dataset 7"}` | Foreign or unknown parent |
| `409 {"error":"parent version 9c4f… is tombstoned"}` | Lineage cannot extend a deleted version |
| `409 {"error":"payload sha256/… is not in the store; the example would be born dangling"}` | A `$payload` reference whose bytes do not exist. An example must not be born dangling — "examples carry their own copies" would be a lie at birth |
| `409 {"error":"payload sha256/… is pending erasure; the promotion conflicts with it"}` | The reference is being erased right now |

### `GET /v1/datasets/{id}/versions/{vid}`

The version's manifest **and** its example bodies, in manifest order:

```json
{"schema":1,"tenant":"acme","dataset_id":1,"version_id":"9c4f…","examples":[["ex-1","63ff05…"]],"created_unix_ns":1755200000000000000,"bodies":[{"example_id":"ex-1","digest":"63ff05…","body":{"input":{"prompt":"why"},"expected":"because","split":"test"}}]}
```

Three answers, deliberately distinct: `200` with manifest and bodies; `410`
with the tombstone when the version was logically deleted — the content is
withheld, the fact of the deletion is not; `404
{"error":"no such dataset version"}` when it never existed (or belongs to
another tenant).

### `POST /v1/datasets/{id}/versions/{vid}/tombstone`

**Requires the `admin` scope**, like erasure: a version tombstone is a
deletion, and a credential minted to write telemetry must not be able to
perform it. **Body.** `{"reason": "..."}`, optional; the reason is recorded
verbatim.

```json
{"tombstoned":true,"already":false,"version_id":"9c4f…"}
```

Idempotent — tombstoning twice answers `"already":true`.

This is **logical** deletion with defined effects:

- fetching the version answers `410` carrying the tombstone;
- dependent experiments **keep working** — their runs and scores are their
  own — and report `dataset_version_deleted: true` so a reader knows the
  dataset content is no longer served;
- new experiments against the version are refused with `409`;
- scores are untouched;
- example bodies stay in the log, and their `$payload` references keep
  counting as live, until a future eval compaction reclaims
  version-unreachable bodies. To destroy promoted *content*, tombstone the
  version and erase the payload — see
  [Erasure](#erasure).

### `POST /v1/experiments`

**Body.** `{"dataset_id": 1, "dataset_version": "9c4f…", "name": "...",
"config": {...}}` — `name` and `config` optional; `config` is free JSON
(model, prompt hash, temperature, …) recorded verbatim.

```json
{"experiment_id":1}
```

Experiment ids are monotonic and never reused, like dataset ids. The tenant
is inherited from the dataset.

**Errors.** `400 {"error":"no dataset 7"}`;
`400 {"error":"version 9c4f… is not a version of dataset 7"}`;
`409 {"error":"version 9c4f… is tombstoned; new experiments cannot run
against it"}`; `409` while an erasure of the tenant is pending.

### `GET /v1/experiments`

Accepts `?dataset_id=` and `?tenant=`, both optional narrowings.

```json
{"experiments":[{"schema":1,"experiment_id":1,"tenant":"acme","dataset_id":1,"dataset_version":"9c4f…","name":"prompt-v2","created_unix_ns":1755200000000000000,"dataset_version_deleted":false,"run_count":12}]}
```

### `GET /v1/experiments/{id}`

One experiment in the same shape, or `404 {"error":"no such experiment"}`.
`dataset_version_deleted` reports whether the version it ran against has been
tombstoned since; `run_count` counts recorded task runs.

### `POST /v1/experiments/{id}/runs`

Records one task run: the experiment→trace link. The harness runs the task
and ingests the trace as ordinary telemetry; this records which example the
trace answers.

**Body.** `{"example_id": "ex-1", "trace_id": "trace-9", "span_id":
"span-3"}` — `span_id` optional, when the harness knows the run's span.

```json
{"recorded":true}
```

Append-only, duplicates legal: a retried example is two runs.

**Errors.** `400 {"error":"run trace_id is empty"}`;
`400 {"error":"no experiment 7"}`; `400 {"error":"example ex-1 is not in
experiment 7's dataset version"}`; `409` while an erasure of the tenant is
pending.

### `GET /v1/experiments/{id}/runs`

```json
{"runs":[{"schema":1,"tenant":"acme","experiment_id":1,"example_id":"ex-1","trace_id":"trace-9","span_id":"span-3","created_unix_ns":1755200000000000000}]}
```

`404` for an unknown (or foreign) experiment.

### `GET /v1/experiments/{id}/scores`

The experiment's score annotations, newest first — the same records
[`GET /v1/annotations?experiment_id=`](#get-v1annotations) returns. Accepts
`?limit=`.

```json
{"scores":[{"trace_id":"trace-9","span_id":"span-3","experiment_id":1,"example_id":"ex-1","name":"groundedness","value":0.9,"source":"eval:nightly","comment":"","timestamp_ns":1755200000000000000}]}
```

### `GET /v1/experiments/{id}/summary`

Per-name score distributions over one experiment.

```json
{"experiment_id":1,"examples_total":12,"scores":[{"name":"groundedness","count":12,"examples_scored":12,"mean":0.84,"min":0.2,"max":1.0,"p50":0.9,"p95":1.0},{"name":"pass","count":12,"examples_scored":12,"mean":0.75,"min":0.0,"max":1.0,"p50":1.0,"p95":1.0,"true_rate":0.75}]}
```

| Field | Description |
|---|---|
| `examples_total` | Examples in the experiment's dataset version manifest |
| `count` / `examples_scored` | Scores counted after dedup — one per scored example |
| `mean` / `min` / `max` / `p50` / `p95` | Over numeric values; booleans count as 0/1. Omitted when no value is numeric |
| `true_rate` | Fraction of boolean scores that were `true`; omitted when none were boolean |

**Scores deduplicate per `(example_id, name)` before any statistic**: the
highest `(timestamp_ns, trace_id, span_id)` wins — the store's
last-write-wins character applied to an append-only log, so a re-scored
example moves a number rather than double-counting, and history stays in the
log.

### `GET /v1/experiments/diff`

Experiment-over-experiment comparison: `?base=` and `?candidate=` name the
two experiment ids, and scores join on `(example_id, name)` after the same
dedup as the summary.

```json
{"base":1,"candidate":2,"scores":[{"name":"groundedness","mean_base":0.84,"mean_candidate":0.91,"delta":0.07,"improved":["ex-3","ex-7"],"regressed":["ex-1"],"unchanged":9,"only_base":[],"only_candidate":["ex-12"]}]}
```

Per name: `improved` and `regressed` list example ids whose value moved,
`unchanged` counts the rest scored in both, `only_base` / `only_candidate`
list examples scored on one side only. Numeric values compare
**higher-is-better, with booleans as 0/1** — a documented convention, not
configurable in this milestone; it is what makes "pass" diffable at all.
Numeric *strings* are deliberately not coerced: scores are machine-written,
and a writer that stringifies numbers should hear about it early.

**Errors.** `400 {"error":"diff needs base= and candidate= experiment ids"}`;
`404 {"error":"no such experiment"}` when either side is unknown or foreign.

---

## Payloads

### `GET /v1/payloads/{reference}`

Returns the raw bytes of an offloaded payload. `{reference}` is the full
`sha256/<hex>` value from a span's `$payload` field, percent-decoded from the
path — the `/` between `sha256` and the hex digest is part of it.

```sh
curl http://localhost:8080/v1/payloads/sha256/29927e273accc68286005017f7fa6e4f27bddb4db3083ff8b8d4c3667905b7fa
```

**Response `200`.** `Content-Type: application/octet-stream` with the payload
bytes. Traza does not record the original media type; interpret it using the
attribute the reference came from.

For an unbound credential the full hash **is** the capability: knowing it
means having read a span that disclosed it. For a **tenant-bound** credential
that argument fails across the boundary — the hash of guessable content is
computable, and a `200` would confirm another tenant stored those exact bytes
— so a bound fetch must additionally prove **reachability**: some span or
dataset example of its own tenant carries the reference. An unreachable
payload answers `404` exactly as an absent one does. See
[administration § Payload fetch across tenants](../operations/administration.md#payload-fetch-across-tenants).

**Errors.** `404 {"error":"payload not found"}`.
`400 {"error":"query deadline exceeded: ..."}` when a bound fetch's
reachability probe exhausts
[`--query-deadline-ms`](../configuration.md#server-flags). `503` on store
failure.

---

## Tenants

### `GET /v1/tenants`

Per-tenant usage accounting: one exact fold over a pinned snapshot. A bound
credential gets exactly its own row; an unbound one gets every tenant's, the
default tenant included as `"tenant":""`.

```sh
curl http://localhost:8080/v1/tenants
```

```json
{"tenants":[{"tenant":"acme","spans":412,"traces":37,"bytes_approx":1048576,"payload_bytes_approx":300000,"first_start_ns":1700000000000000000,"last_end_ns":1700003600000000000}]}
```

| Field | Type | Description |
|---|---|---|
| `tenant` | string | The tenant; empty is the default tenant |
| `spans` | integer | Logical spans currently visible — LWW-resolved, superseded versions excluded. What the tenant sees, not what disk holds |
| `traces` | integer | Distinct traces among them |
| `bytes_approx` | integer | Approximate serialized size of those spans, inline content only |
| `payload_bytes_approx` | integer | Bytes of distinct offloaded payloads the tenant's spans reference, from the reference objects' recorded sizes. A blob shared across tenants counts for **every** referencing tenant — for a quota question, each of them is holding the store to those bytes |
| `first_start_ns` | integer | Earliest span start |
| `last_end_ns` | integer | Latest span end |

**This is an exact fold and it is O(store)** — it decodes the tenant's
visible spans to count them, on demand. It is an accounting answer, not a
metric to poll per minute against a hundred-million-span corpus; for
continuous monitoring use [`/v1/metrics`](#get-v1metrics), and reach for this
route when the question is "what does each tenant hold right now".

**Errors.** `503` on store failure.

### `GET /v1/usage`

What each tenant has **sent**, against its limits: monotonic counters kept as
batches are acknowledged, answered in O(tenants) without reading a segment.
Where [`/v1/tenants`](#get-v1tenants) answers "what does it hold now" (expiry
and erasure shrink that), this answers "what has it been charged for".

A bound credential gets exactly its own row (a `ro` token suffices — this is
not an operator surface); naming another tenant is `403`. An unbound one gets
every tenant that has counters or limits, or one tenant with `?tenant=` (empty
selects the default tenant). A tenant with neither gets a zero row when asked
for by name.

```sh
curl -H "Authorization: Bearer $ACME_TOKEN" http://localhost:8080/v1/usage
```

```json
{"as_of_ns":1792061745453808000,"tenants":[{"lifetime":{"batches":1,"bytes":209,"rejected":0,"spans":1},"limits":{"ingest_paused":false,"monthly_bytes":10000000000,"monthly_spans":1000000,"ttl_seconds":2592000},"month":{"bytes":209,"spans":1,"window":"2026-10"},"state":"ok","tenant":"acme"}]}
```

| Field | Description |
|---|---|
| `lifetime.spans` | Spans admitted since the counters began |
| `lifetime.bytes` | Their size: each span's native JSON after tenant stamping and **before** payload offload — what the client sent, in Traza's encoding. An OTLP batch is charged its native-JSON size, not its protobuf wire size |
| `lifetime.batches` | Ingest calls that admitted at least one of the tenant's spans |
| `lifetime.rejected` | Spans refused by a quota or a pause. Admission-cap refusals (`400`) are not counted here |
| `month.window` | The current UTC calendar month, `YYYY-MM`. It only moves forward: a batch acknowledged across a month boundary out of order, or under a wall clock stepped back, is charged to the newest month already counted rather than resetting it |
| `month.spans`, `month.bytes` | The same counts for that month; zero once it turns |
| `limits` | The tenant's [limits document](#post-v1admintenantstenantlimits) (defaults when never set) |
| `state` | `paused`; else `capped` when a monthly limit is reached; else `warning` at 80% or more of any monthly limit; else `ok` |

Spans a pending erasure suppressed at ingest are not admitted and not
charged. **The counters are written to `usage.json` every five seconds and on
every flush**, and only when they changed. The five-second writer is a server
thread of its own, so a long merge or expiry pass does not delay it; a crash
loses the counts since the last completed write — normally five seconds'
worth, plus however long that write's fsync took — an undercount, never an
overcount. They are kept outside generations: a restore does not rewind them.

**Errors.** `400` for an unknown query parameter; `403` when a bound
credential names another tenant.

### `POST /v1/admin/tenants/{tenant}/limits`

Replaces a tenant's limits document — the **whole** document: a field left out
returns to its default. Unbound `admin` only. `{tenant}` is a tenant name, or
`_default` for the default tenant.

```sh
curl -X POST http://localhost:8080/v1/admin/tenants/acme/limits \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"monthly_spans":1000000,"monthly_bytes":10000000000,"ingest_paused":false,"ttl_seconds":2592000}'
```

```json
{"ingest_paused":false,"monthly_bytes":10000000000,"monthly_spans":1000000,"tenant":"acme","ttl_seconds":2592000}
```

| Field | Type | Default | Effect |
|---|---|---|---|
| `monthly_spans` | integer or null | `null` | Spans the tenant may ingest per UTC calendar month; `null` is unlimited |
| `monthly_bytes` | integer or null | `null` | Bytes (counted as in [`/v1/usage`](#get-v1usage)) per month; `null` is unlimited |
| `ingest_paused` | bool | `false` | Refuse every batch for the tenant with `429`. Reads, exports and erasure are unaffected |
| `ttl_seconds` | integer or null | `null` | Retention for this tenant, overriding `--tenant-ttl` and `--ttl-seconds`. `0` keeps everything; `null` falls back to the flags |

The response is the stored document plus `tenant` (`""` for the default
tenant). It is durable before the response is sent (`limits.json`, rewritten
atomically and fsynced, outside generations). Quotas and the pause apply from
the next batch; a TTL change applies at the next retention pass, which the
server runs every minute.

**Errors.** `400` for an invalid tenant, an unknown field, or a value of the
wrong type; `403` for any credential that is not an unbound `admin`; `415`
when the body is not declared `application/json`.

### `GET /v1/admin/tenants/{tenant}/limits`

The tenant's document, in the same shape; the defaults (no limits) when it was
never set. Unbound `admin` only.

```json
{"ingest_paused":false,"monthly_bytes":null,"monthly_spans":null,"tenant":"acme","ttl_seconds":null}
```

---

## Credentials and identity

### `GET /v1/whoami`

What the calling credential is. Any valid credential may ask, and in open
mode the server answers too. Not an operator surface.

```sh
curl -H "Authorization: Bearer $ACME_TOKEN" http://localhost:8080/v1/whoami
```

```json
{"auth":"token","credential":{"id":"cr_b218d515889e9779","label":""},"operator":false,"scope":"rw","tenant":"acme"}
```

| Field | Description |
|---|---|
| `auth` | `token`, or `open` when no credential exists anywhere |
| `scope` | `ro`, `rw` or `admin` (`admin` in open mode, which permits everything) |
| `tenant` | The binding, or `null` for an unbound credential |
| `operator` | `true` for an unbound credential (or open mode): it may use the operator surfaces. Scope does not decide this; the absence of a binding does |
| `credential` | `{"id","label"}` of the credential that authenticated; `null` in open mode |

An environment credential's id is derived from its token's digest, so it is
the same after every restart.

### `POST /v1/admin/credentials`

Creates a credential while the server runs. Unbound `admin` only — or open
mode, which is how the first credential of a store is minted: from the moment
one exists, every request must authenticate. **The first credential must
itself be an unbound `admin`** (`409` otherwise): any other would switch
authentication on with no credential able to manage credentials. In open mode
the request must not come from a non-loopback browser page (see
[Conventions](#conventions)). **Exactly one bootstrap wins:** whether the
store is still open is decided together with the create, so of two
unauthenticated requests racing to mint the first credential one gets `201`
and the other `401` — it creates nothing and must come back with a
credential.

```sh
curl -X POST http://localhost:8080/v1/admin/credentials \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"scope":"rw","tenant":"acme","label":"acme collector","expires_in_seconds":2592000}'
```

`201`:

```json
{"created_at_ns":1792061745307152000,"expires_at_ns":1794653745307152000,"id":"cr_1b4e75684650c646","label":"acme collector","prefix":"trz_c168ab97","scope":"rw","tenant":"acme","token":"trz_c168ab97dd0fc7340e749c2d449cc3cf846c2e13fe11b0f86590342816360821"}
```

| Body field | Type | Effect |
|---|---|---|
| `scope` | string, required | `ro`, `rw` or `admin` |
| `tenant` | string | Bind to this tenant; omit for an unbound credential |
| `label` | string | A note for the credential list, at most 256 bytes |
| `expires_in_seconds` | integer | Lifetime from now, at least 1; omit for no expiry |
| `token` | string | Register this token instead of minting one: at least 24 bytes, no whitespace, control characters or commas. For a control plane reconciling a credential it already issued. **It must be high-entropy** (at least 128 random bits): the store protects it at rest with nothing but an unsalted SHA-256, and only its length is checked |

**`token` is in this response and nowhere else, ever.** The store keeps only
its SHA-256, so it cannot be shown again; lose it and revoke the credential.
A minted token is `trz_` plus 64 hex digits from the operating system's
random source (`/dev/urandom`; where that does not exist, minting fails with
`503` and a supplied `token` still works). `prefix` is the part safe to show
later: at most a quarter of the token and never more than 12 characters —
`trz_` and eight hex digits for a minted one.

**Errors.** `400` naming the offending field (unknown fields are refused);
`409` when the token is already registered — live, expired, revoked, or in
`TRAZA_TOKENS`: a token is never reused, so a revoked one cannot come back —
or when the store has no credential yet and this one is not an unbound
`admin`; `401` for an unauthenticated request that lost the bootstrap (above);
`403` for anything but an unbound `admin`, and in open mode for a
non-loopback browser `Origin`; `415` when the body is not declared
`application/json`; `503` when the credential log could not be written. A
failed write is truncated away before the `503`, so nothing was created and
retrying the same request is safe; if even that truncation fails, every
create and revoke answers `503` until the server restarts and heals the log,
while authentication carries on unaffected.

### `GET /v1/admin/credentials`

Every credential: environment ones first, then runtime ones in creation
order. Never a token or a digest. Unbound `admin` only.

```json
{"credentials":[{"created_at_ns":1792061745172064000,"expires_at_ns":null,"id":"cr_5cd4bd2cc16c3087","label":"","last_used_at_ns":1792061745321685000,"prefix":"root-","revoked_at_ns":null,"scope":"admin","source":"env","tenant":null},{"created_at_ns":1792061745307152000,"expires_at_ns":1794653745307152000,"id":"cr_1b4e75684650c646","label":"acme collector","last_used_at_ns":null,"prefix":"trz_c168ab97","revoked_at_ns":null,"scope":"rw","source":"api","tenant":"acme"}]}
```

`source` is `env` (from `TRAZA_TOKENS`; `created_at_ns` is when this process
loaded it) or `api`. `last_used_at_ns` is kept **in memory only**: a restart
forgets it, because recording it durably would put an fsync on every
authenticated request.

### `POST /v1/admin/credentials/{id}/revoke`

Revokes a runtime credential: from the next request its token is `401`, like
an unknown one. The revocation is fsynced before the response, so it survives
a crash. Idempotent — revoking again answers with the original instant.
Unbound `admin` only.

```json
{"id":"cr_1b4e75684650c646","revoked_at_ns":1792061745365355000}
```

**Errors.** `404` for an unknown id;
`409 {"error":"env credentials are revoked by restarting without them"}` for
an environment credential; `503` when the revoke record could not be written
— undone, as for a create, so the token still authenticates and the revoke
can be retried.

---

## Run fuses

Per-run limits — cost, tokens, steps, LLM and tool calls, duration, repeated
steps, consecutive failures — and the verdict they give: `continue`, `warn`
at 80% of any limit, and at 100% the policy's action, where `stop` is sticky
until reset. The [guide](guard.md) explains the model; this is the wire.

Every route answers `409` when the server runs with `--no-fuses`. A run id in
a path is percent-decoded, so a run containing `/` travels as `%2F`; it is at
most 1,024 bytes (`400` beyond). On the
single-run routes `?tenant=` selects the run's tenant for an unbound
credential (absent: the default tenant); a bound credential is forced to its
own, and naming another is `403`. Unknown query parameters and unknown body
fields are `400`.

**The verdict document** is what every run route answers:

| Field | Type | Description |
|---|---|---|
| `run` | string | The run id |
| `tenant` | string | The run's tenant; empty is the default tenant |
| `verdict` | string | `continue`, `warn` or `stop` |
| `reasons` | string[] | For a stop, the reasons it tripped with; otherwise the limits reached or approached now. Empty for `continue` |
| `policy` | string | The policy the run is held to; `global` for the server default |
| `usage` | object | `steps`, `llm_calls`, `tool_calls`, `tokens`, `cost_usd`, `errors`, `error_streak`, `duration_ns`, and `top_repeat` (`{fingerprint, count, example: {trace_id, span_id}}` or `null`) |
| `limits` | object | The limits in force: the policy's, with the run's grant over them. Absent limits are unbounded |
| `grant` | object \| null | Limits raised for this run by resets |
| `tripped_at_ns` | integer \| null | When the stop tripped |
| `tripped_by` | string \| null | `policy` or `manual`, for a recorded stop |
| `evidence` | object[] | `{trace_id, span_id}` of the span that tripped a stop, and of the repeated step when repeats are a reason |
| `source` | string | `live` from memory, `rebuilt` when answering read the run's stored spans |

### `POST /v1/fuses/check`

The pre-call gate. Needs only the `ro` scope. **Body:**
`{"run": string, "tenant"?: string, "next"?: {"est_cost_usd"?: number,
"est_tokens"?: integer, "tool"?: string}}`.

Answers the verdict document with `next` added to the run's usage
**hypothetically**: one step, an LLM call unless `tool` is named, plus the
estimates. A step that would go **past** a stop limit answers `stop`. Nothing
about the run changes — `usage` is what it has recorded, and a hypothetical
stop has no `tripped_by` — and a stop is still a `200`: the verdict is the
answer. A run with no spans is a fresh run with zero usage.

```sh
curl -X POST http://127.0.0.1:8080/v1/fuses/check \
  -d '{"run": "sess-9c21", "next": {"est_cost_usd": 0.40}}'
```

```json
{"evidence":[{"span_id":"b1","trace_id":"4bf92f3577b34da6a3ce929d0e0e4736"}],"grant":null,"limits":{"max_cost_usd":2.0,"max_error_streak":3,"max_repeats":4,"max_steps":40},"policy":"support","reasons":["max_cost_usd 2 would be exceeded by the next step: $1.7 spent + $0.4 estimated"],"run":"sess-9c21","source":"live","tenant":"","tripped_at_ns":null,"tripped_by":null,"usage":{"cost_usd":1.7,"duration_ns":900000000,"error_streak":0,"errors":0,"llm_calls":1,"steps":1,"tokens":1920,"tool_calls":0,"top_repeat":null},"verdict":"stop"}
```

**Errors.** `400` for a body that does not parse, an empty, bare `trace:` or
over-long run id, or a negative `est_cost_usd`; `400` if rebuilding the run
exhausted `--query-deadline-ms` and what memory holds of it does not already
stop it (a kill switch, a stop latched live or parked, or a `next` step past
a limit on what is known still answers `stop`); `400` for a run with more
than 100,000 stored spans whose earliest 100,000 — all a rebuild reads — do
not stop it, since part of a run's history can establish a stop but never
clear one (see [the guide](guard.md#what-is-durable-and-what-is-rebuilt));
`409` in the rare case that erasures kept overlapping the run's rebuild
(each overlap discards what the rebuild read, so retry once they settle).
None of these is a verdict, so a hook that fails closed blocks on them.

### `GET /v1/fuses/runs/{run}`

The run's verdict document. The first question about a run rebuilds it
from the store (`"source": "rebuilt"`), since the process cannot know what
was stored before it began following the run; later ones are answered from
memory (`"live"`). A run no span carries answers `continue` with zero usage,
and asking about one holds no state for it.

```sh
curl http://127.0.0.1:8080/v1/fuses/runs/sess-7f3a
```

```json
{"evidence":[{"span_id":"r2","trace_id":"4bf92f3577b34da6a3ce929d0e0e4736"}],"grant":null,"limits":{"max_cost_usd":2.0,"max_error_streak":3,"max_repeats":4,"max_steps":40},"policy":"support","reasons":["max_repeats 4 reached: tool:search_orders#465a6bab9a1c447d repeated 4 times"],"run":"sess-7f3a","source":"rebuilt","tenant":"","tripped_at_ns":1792062221822334000,"tripped_by":"policy","usage":{"cost_usd":0.42,"duration_ns":4250000000,"error_streak":0,"errors":0,"llm_calls":1,"steps":5,"tokens":1920,"tool_calls":4,"top_repeat":{"count":4,"example":{"span_id":"r2","trace_id":"4bf92f3577b34da6a3ce929d0e0e4736"},"fingerprint":"tool:search_orders#465a6bab9a1c447d"}},"verdict":"stop"}
```

**Errors.** As for the check.

### `GET /v1/fuses/runs`

Runs the guard holds, most recently active first: `{"runs": [verdict
documents]}`. That is every live run plus every stopped run parked out of the
live set by eviction (see
[the guide](guard.md#what-is-durable-and-what-is-rebuilt)).
`?state=active|warn|stop` (default `active`, all of them), `?limit=`
(default 100, at most 1000), `?tenant=` (an unbound credential naming none
sees every tenant). Answered from memory alone — a listing never rebuilds —
so each document is `"source": "live"`, a run followed only partly lists
with what this process has seen, and a parked run with its usage as last
seen live.

```sh
curl 'http://127.0.0.1:8080/v1/fuses/runs?state=stop'
```

```json
{"runs":[{"evidence":[],"grant":null,"limits":{"max_cost_usd":2.0,"max_error_streak":3,"max_repeats":4,"max_steps":40},"policy":"support","reasons":["manually tripped: customer escalated; take over by hand"],"run":"sess-9c21","source":"live","tenant":"","tripped_at_ns":1792062221860163000,"tripped_by":"manual","usage":{"cost_usd":1.7,"duration_ns":900000000,"error_streak":0,"errors":0,"llm_calls":1,"steps":1,"tokens":1920,"tool_calls":0,"top_repeat":null},"verdict":"stop"}]}
```

### `POST /v1/fuses/runs/{run}/trip`

The kill switch: stops the run until it is reset, whatever its usage,
durably. **Body** (optional): `{"reason": string}`, at most 1,024 bytes,
stored as given in `fuses.jsonl` until the run's session or tenant is erased
— keep personal data out of it. Answers the verdict document.

**Errors.** `409` when the tenant already holds durable controls (a kill
switch, a reset or a grant) for 10,000 runs and every one of them is an
active kill switch; below that, a new run takes the place of the run reset
longest ago, whose grant and reset stop applying.

```sh
curl -X POST http://127.0.0.1:8080/v1/fuses/runs/sess-9c21/trip \
  -d '{"reason": "customer escalated; take over by hand"}'
```

```json
{"evidence":[],"grant":null,"limits":{"max_cost_usd":2.0,"max_error_streak":3,"max_repeats":4,"max_steps":40},"policy":"support","reasons":["manually tripped: customer escalated; take over by hand"],"run":"sess-9c21","source":"live","tenant":"","tripped_at_ns":1792062221860163000,"tripped_by":"manual","usage":{"cost_usd":1.7,"duration_ns":900000000,"error_streak":0,"errors":0,"llm_calls":1,"steps":1,"tokens":1920,"tool_calls":0,"top_repeat":null},"verdict":"stop"}
```

### `POST /v1/fuses/runs/{run}/reset`

Clears the run's stop — a policy stop or the kill switch — and its error
streak and repeat counts, durably. **Body** (optional):
`{"grant": {limits}}` raises the run's limits to the absolute values named,
merged over earlier grants; it takes the [policy limit](#post-v1fusespolicies)
fields and the same validation. Cumulative usage is not reset, so a run still
over a cumulative limit answers `stop` again at once. Answers the verdict
document. **Errors** as for the trip: the same 10,000-run bound applies. And
as for [the check](#post-v1fusescheck) when the run's verdict cannot be given
afterwards — a rebuild out of time, or a run too long to read whole, that
nothing stops once reset — in which case the reset is recorded all the same.

```sh
curl -X POST http://127.0.0.1:8080/v1/fuses/runs/sess-7f3a/reset \
  -d '{"grant": {"max_repeats": 8}}'
```

```json
{"evidence":[],"grant":{"max_repeats":8},"limits":{"max_cost_usd":2.0,"max_error_streak":3,"max_repeats":8,"max_steps":40},"policy":"support","reasons":[],"run":"sess-7f3a","source":"live","tenant":"","tripped_at_ns":null,"tripped_by":null,"usage":{"cost_usd":0.42,"duration_ns":4250000000,"error_streak":0,"errors":0,"llm_calls":1,"steps":5,"tokens":1920,"tool_calls":4,"top_repeat":null},"verdict":"continue"}
```

### `POST /v1/fuses/policies`

Creates a policy, or replaces the tenant's policy of the same name (keeping
its `id` and `created_at_ns`). Live runs of the tenant are re-judged at once.
**Body:**

| Field | Type | Description |
|---|---|---|
| `name` | string | 1–64 bytes, unique within the tenant. `default` is the tenant's fallback; `global` is reserved |
| `tenant` | string | Optional; a bound credential is forced to its own |
| `match` | object | `{"service": string}` (1–256 bytes) — runs whose first span, by start time, came from that service. Required unless the name is `default`, which must not have one |
| `limits` | object | Any of `max_cost_usd`, `max_tokens`, `max_steps`, `max_llm_calls`, `max_tool_calls`, `max_duration_seconds`, `max_repeats`, `max_error_streak`. Each positive; `max_repeats` at least 2. Absent is unbounded, and nothing is inherited from `default` or `global` |
| `action` | string | `stop` or `warn` (default `warn`) |

```sh
curl -X POST http://127.0.0.1:8080/v1/fuses/policies -d '{
  "name": "support", "match": {"service": "support-agent"},
  "limits": {"max_cost_usd": 2.0, "max_steps": 40, "max_repeats": 4, "max_error_streak": 3},
  "action": "stop"}'
```

```json
{"action":"stop","created_at_ns":1792062221773128000,"id":"fp_58ed88d43fc762ba","limits":{"max_cost_usd":2.0,"max_error_streak":3,"max_repeats":4,"max_steps":40},"match":{"service":"support-agent"},"name":"support","tenant":""}
```

**Errors.** `400` naming the defect (a policy that matches nothing, a zero
limit, an unknown field); `403` for a bound credential naming another tenant;
`409` when creating (not replacing) a policy would give the tenant more than
256.

### `GET /v1/fuses/policies`

`{"policies": [...], "global": {...}}`: the stored policies by tenant then
name (a bound credential sees its own; `?tenant=` narrows for an unbound
one), and the server-wide default every run without a matching or `default`
policy gets.

```json
{"global":{"action":"warn","limits":{"max_repeats":12,"max_steps":50},"name":"global"},"policies":[{"action":"stop","created_at_ns":1792062221773128000,"id":"fp_58ed88d43fc762ba","limits":{"max_cost_usd":2.0,"max_error_streak":3,"max_repeats":4,"max_steps":40},"match":{"service":"support-agent"},"name":"support","tenant":""}]}
```

### `POST /v1/fuses/policies/{id}/delete`

Deletes a policy, durably; live runs of its tenant are re-judged at once (a
sticky stop stays). Answers `{"deleted": true, "policy": {...}}`, or `404`
when no such policy exists — another tenant's policy, to a bound credential,
included.

### `GET /v1/fuses/events`

Verdict transitions as they happen, by cursor. `?after=<cursor>` (absent:
everything retained), `?wait_ms=0..30000` (default 0) to wait for an event
when none is there yet, `?tenant=` (an unbound credential naming none sees
every tenant).

```sh
curl 'http://127.0.0.1:8080/v1/fuses/events?wait_ms=0'
```

```json
{"events":[{"at_ns":1792062221822334000,"cursor":"1792062221423179000.1","kind":"stop","policy":"support","reasons":["max_repeats 4 reached: tool:search_orders#465a6bab9a1c447d repeated 4 times"],"run":"sess-7f3a","tenant":""},{"at_ns":1792062221847207000,"cursor":"1792062221423179000.2","kind":"warn","policy":"support","reasons":["max_cost_usd 2 at 85%: $1.7 spent"],"run":"sess-9c21","tenant":""},{"at_ns":1792062221850690000,"cursor":"1792062221423179000.3","kind":"reset","policy":"support","reasons":["reset with grant {\"max_repeats\":8}"],"run":"sess-7f3a","tenant":""},{"at_ns":1792062221860163000,"cursor":"1792062221423179000.4","kind":"manual_stop","policy":"support","reasons":["manually tripped: customer escalated; take over by hand"],"run":"sess-9c21","tenant":""}],"next":"1792062221423179000.4"}
```

`kind` is `warn` (a run entered `warn`, or a `warn` policy's limit was
then reached; not repeated until the run returns to `continue` or is reset),
`stop` (a policy stopped it), `manual_stop` or `reset`. Pass `next` back as `after`; it advances past events a tenant filter
skipped, so a filtered reader keeps pace with the feed. A read returns as soon
as a matching event exists, at most 1,000 at a time.

The feed is an in-memory ring of 4,096 events, and a cursor is
`<epoch>.<seq>` like the [live tail's](#get-v1tail): the epoch names the
process. A cursor the feed can no longer honour — evicted, or from before a
restart — answers everything still retained with `"gap": true`, meaning
events may have been missed in between.

**Errors.** `400` for a malformed cursor or a `wait_ms` above 30,000.

---

## Erasure

Targeted deletion by **subject**, with a receipt to prove it. TTL removes
spans by age; erasure removes them because someone is entitled to have them
gone, and the difference is what must be demonstrable afterwards. The
operational story — what remains on purpose, pins, re-delivery — is in
[administration § Erasure](../operations/administration.md#erasure-deletion-with-a-receipt).

There is deliberately **no MCP tool for erasure**: the agent-facing surface
stays read-only, so stored adversarial text has no deletion verb to actuate.

### `POST /v1/erasures`

**Requires the `admin` scope** when authentication is on — an `rw` token gets
`403 {"error":"forbidden","required":"admin"}`. Erasure is a POST like any ingest, so
the method rule cannot distinguish it, and it must be distinguished: a
credential minted to write telemetry must not be able to destroy it. See
[administration § Authentication](../operations/administration.md#authentication).

Erases one subject and blocks until the erasure settles. The `200` is the
acknowledgement, and it means the whole sequence ran, in an order chosen so
each artifact stays true: barrier → purge → confirm → checkpoint → settle.
The intent is fsynced into the tombstone log; from that moment covered spans
are dropped at admission **before payload offloading** (acknowledged, not
stored, no bytes written; counted in
`traza_erasure_spans_suppressed_total` and reported in the ingest response),
and covered annotations are dropped at [`POST /v1/annotations`](#post-v1annotations)
the same way. The purge and a confirm pass then rewrite the buffer,
write-ahead log, segments (superseded versions included) and annotation log,
delete payload files reference-aware, and remove what the
[run fuses](guard.md#limitations) hold about the subject. Only after every rewrite does the
checkpoint publish — durable at the `CURRENT` rename — so the generation the
settle record cites digests exactly the store the erasure left behind, and
`GET /v1/verify` holds against it. The cut is exact: **every span
acknowledged before `settled_unix_ns` is erased or was never stored.**

```sh
curl -X POST http://localhost:8080/v1/erasures \
  -H 'Content-Type: application/json' \
  -d '{"subject": {"kind": "trace", "trace_id": "trace-1"}}'
```

| Subject | Fields | Erases |
|---|---|---|
| `trace` | `trace_id`, `tenant` | Every span of **one tenant's** trace, its annotations, payload bytes no survivor references |
| `span` | `trace_id`, `span_id`, `tenant` | One span by primary key, all physical versions |
| `session` | `session_id`, `tenant` | Every span of one tenant resolving to the session, across all recognized session keys |
| `payload` | `reference` | The `sha256/<hex>` file, plus a rewrite of every referencing span dropping its inline preview. Carries **no tenant** — content addressing is store-global — and is reserved for unbound admin credentials |
| `tenant` | `tenant` (non-empty) | Everything the tenant owns: its spans across every domain, its annotations and scores, its datasets, versions, examples and experiments, and reference-aware deletion of the payload bytes its spans and examples held |

On trace/span/session subjects `tenant` is optional and **empty means the
default tenant, never "all tenants"** — two tenants sharing a trace id are
two subjects, and erasing one leaves the other untouched, which is the
primary key doing its job. Every subject's `tenant` also accepts `$tenant`,
so the span's reserved spelling cannot silently aim a deletion at the default
tenant. The `tenant` subject requires a non-empty name:
the default tenant is every store that never configured tenancy, and "erase
it whole" is "erase the store", which is not an API — narrower subjects
express every legitimate deletion
(`400 {"error":"a tenant subject names a non-empty tenant"}`).

**Tenant subjects record no span keys, deliberately.** Every other subject is
resolved at request time to the concrete `(tenant, trace_id, span_id)` keys
it covers, bounded by the subject's own size; a tenant's key set is
unbounded, the mask and the purge cover by predicate instead, and for a whole
tenant **the settle time is the re-delivery line**: any of the tenant's data
ingested before `settled_unix_ns` is erased or was never stored, anything
after is new data. While a tenant erasure is pending, eval mutations for that
tenant are refused with
`409 {"error":"tenant acme has an erasure pending; retry after it settles"}`
rather than raced.

**A bound admin erases its own tenant and nothing else** — the
tenant-subject arm very much included, or a per-tenant admin could destroy a
neighbour wholesale. Naming a foreign tenant answers
`403 {"error":"this credential is bound to a different tenant","required":"unbound"}`; a payload
subject answers `403 {"error":"payload subjects are store-global; erasing one
requires an unbound admin credential"}`. Bound credentials also see only
their own tenant's records at [`GET /v1/erasures`](#get-v1erasures), and a
foreign erasure id answers `404`.

**Response `200`.** The erasure record — id, subject (payload hashes are
canonicalized to lowercase before anything is resolved or recorded) — plus
its `settle` block: `spans_removed`, `spans_redacted`,
`annotations_removed`, `eval_records_removed` (datasets, versions, examples,
experiments, runs and version tombstones removed from the eval log — nonzero
only for tenant subjects; a tenant's *scores* are annotations and count under
`annotations_removed`), `payloads_removed`, `payloads_retained` (each
retained reference carries its reason — shared content still referenced by
live spans outside the subject is kept, and the receipt says so), and the
`generation` that published the deletion. The counts are the settling pass's
counts: after a crash-resume the first pass's work is already done and cannot
be re-counted, so the authority on absence is always the receipt.

Erase and settle records are written at **schema 2**: `span_keys` are
`(tenant, trace_id, span_id)` triples, and the trace/span/session subjects
carry their tenant. Schema-1 records — two-element `(trace_id, span_id)`
pairs — replay as the default tenant; arity decides the decoding, never a
guess.

[Replays](#post-v1replays) hold copies of recorded answers and tool records
in memory, so erasure reaches them too: from the moment the tombstone is
recorded, every call through a replay holding a covered span, trace, session,
tenant or payload answers `410` with `"type": "replay_erased"`, and the
replay's copy is dropped at that call (or when it expires, at most an hour
after creation, if nothing calls it again).

Reads and ingest continue throughout. From the moment the tombstone is
recorded the subject is invisible to every query — before the rewrites run —
and a crash mid-erasure leaves a pending erasure that masks at the next open
and is finished by the server's maintenance tick. Spans ingested under the
same identifiers **after** the erasure settles are new data: a tombstone is a
barrier, not a ban.

**Errors.** `400` on a malformed body or subject. `503` on store failure.

### `GET /v1/erasures`

Every erasure the tombstone log records, oldest first, each with its `settle`
block or `null` while pending. `GET /v1/erasures/{id}` returns one.

### `GET /v1/erasures/{id}/verify`

The **erasure receipt**: re-checks every domain the subject's bytes could
inhabit and reports the result of each, by name — write buffer, live tail,
write-ahead log, segments, annotations, **eval records**, payload files,
derived caches, pins, generation metadata, the tombstone log itself
(retained by design: the record of the erasure is what this receipt verifies
against), and the [ledger](ledger.md) (retained by design: its erasure entry
carries a digest of the subject, never a trace, span or session id or a
payload reference — though a tenant subject's name is the entry's `tenant`).
Replays are not a domain it walks: their copies live in the server process
rather than the store, and are withdrawn by the replay routes themselves, as
[`POST /v1/erasures`](#post-v1erasures) describes.

The `eval-records` domain sits between annotations and payloads, and what it
reports depends on the subject:

- for **trace/span/session** subjects it lists examples in the subject's own
  tenant's datasets whose provenance or content is traceable to the subject —
  promoted **copies**, which survive source erasure by design. These report
  as `attention` (the receipt stays inconclusive): purging them is a
  deliberate second act — tombstone the version, erase the payload — not a
  side effect of erasing the source;
- for **payload** subjects it lists examples still carrying the reference.
  These report as `retained-by-design` and stay conclusive: after the blob's
  deletion the reference is a dangling **address, not content** — the bytes
  are gone and the version's digests remain valid;
- for **tenant** subjects it re-counts the records the tenant still owns,
  which must be zero.

The `payloads` domain accounts for the union of the references recorded at
resolve time and every disposition the settle recorded. The union matters
most for tenant subjects, whose resolve-time list is deliberately empty (the
purge collects their refs as it walks) — without the settle's lists the
domain would verify nothing and a shared payload's retention would go
unnamed.

```sh
curl http://localhost:8080/v1/erasures/1/verify
```

`result` is `erased` or `not-erased`, computed from what the walk found and
never from what the settle record claims. Matches are classified against the
erase record's resolved keys under one rule for every domain: an erased key
found live again is a **re-delivery** and fails the receipt; a fresh key
matching the subject is **new activity** and is reported without failing it.
A pin created before the erasure still holds the subject's bytes in its
hard-link farm, and the receipt says which pin to release.

The byte-level occurrence scans (the log, rollup sidecars) are
over-approximate on purpose — an identifier quoted in unrelated content
counts — so their findings report as `attention` and are carried in a
separate top-level field: **`conclusive`** is `false` whenever any
over-approximate signal was found and not proven benign. `result` answers
what the semantic walk found; `conclusive` answers whether anything at all
was left ambiguous. The axes disagree in both directions: `erased` but not
`conclusive` says the walk is clean but a scan saw the identifiers
somewhere, while `not-erased` with `conclusive: true` says the walk
definitively found retained bytes — a pinned backup, a re-delivery — with
nothing ambiguous about the finding. A receipt offered as proof should be
`erased` **and** `conclusive`.

The same receipt is available offline, against a directory no live server
owns:

```sh
traza-server verify --erasure 1 --data-dir /var/lib/traza
```

Exits `0` when the receipt is erased and conclusive, `3` when it is erased
but inconclusive, `2` when the erasure did not hold.

**Errors.** `404` for an id the tombstone log does not record. `400` on a
non-numeric id. `503` on store failure. The walk probes segment indexes
rather than decoding the corpus, but it is a real verification, not a cache
read — expect it to cost more than a query.

---

## Ledger and evidence

The ledger is a hash chain over everything the store has committed: one
entry per published generation, published erasure, and external anchor, each
entry's `head` committing to the one before it. An evidence pack gathers the
chain, its verification, retention, activity and erasures into one document
with one digest for a notary to sign. The format, the head formula, a
ten-line script that recomputes every head, and an honest account of what
the chain does and does not prove are in [the ledger guide](ledger.md).

### `GET /v1/ledger`

Entries in chain order, exactly as stored. Unbound credentials only — the
chain carries every tenant's erasure entries.

| Parameter | Meaning |
|---|---|
| `after` | Only entries with `seq` greater than this |
| `limit` | Page size, `1`–`1000`; default `100` |

```sh
curl 'http://localhost:8080/v1/ledger?after=2'
```

```json
{"entries":[
  {"at_ns":1792061585524084000,"generation":3,"head":"eb5610460e9657ed3ec706d3831bbff1ee732b9cc6f3365f55519f769d752ac9","kind":"checkpoint","manifest_sha256":"97bcc77723139e1d84ac46f786ee85152e6be3ce0d35938d797c85b33429b983","prev":"0dee27bf2cb8dd9fb141908e863e72dfb6964515b564eeb27bb824eac34adf45","seq":3},
  {"at_ns":1792061585533710000,"erasure_id":1,"generation":3,"head":"34e64ba565ed353f7db1ab6f1e9611b8815053c45548c22b9827d2c3dfc415e5","kind":"erasure","prev":"eb5610460e9657ed3ec706d3831bbff1ee732b9cc6f3365f55519f769d752ac9","salt":"9f1c27e04b6a5d83c0e2f7a14b98d635","seq":4,"subject_digest":"f6b6db7b0f95581e281ebdcc1eef0310d01ce2a791289e32e6cdddb8e40e7142","subject_kind":"session","tenant":""}],
 "head":{"head":"34e64ba565ed353f7db1ab6f1e9611b8815053c45548c22b9827d2c3dfc415e5","seq":4}}
```

An erasure entry's `generation` is the generation that published it, and its
`salt` is 32 random hex characters inside its head, so a tenant shown only a
stub of the entry (see [`GET /v1/evidence`](#get-v1evidence)) cannot
recompute that head for a guessed subject. `head` and `entries` come from one
snapshot, so no entry in a page is past the head reported with it. Timestamps
exceed 2^53: parse them with an exact integer type (a JavaScript client needs
a lossless parser) or recomputed heads will not match.

A line that does not parse is skipped here and named by
[`GET /v1/ledger/verify`](#get-v1ledgerverify). Errors: `400` for a `limit`
outside `1`–`1000`, a non-numeric `after`, or an unknown parameter.

### `GET /v1/ledger/head`

The chain's tip. Any `ro` credential, bound ones included: a head discloses
no tenant's data, and a tenant may want to anchor it.

```json
{"at_ns":1792061561108805000,"head":"829a2f44e1b9d65b1f8dadd5df1d6a01b2c7b3465771b17db61ac79decb1fd2b","seq":1,"store_id":"66022a6e1d87f73e3ba52202b53d1998"}
```

`store_id` names the chain (32 random hex characters from the genesis
entry); a store restored into an empty directory starts a new chain with a
new id.

### `GET /v1/ledger/verify`

Recomputes every head and link, checks that the latest checkpoint entry
names the live generation's manifest digest, re-digests every file that
manifest names (the [`GET /v1/verify`](#get-v1verify) check), checks every
ledgered erasure against the live tombstone log and every settled erasure in
that log against the chain, and reports any ledger append that failed and has
not been caught up. Unbound credentials only. Always `200` — the verdict is
the body:

```json
{"entries":5,"generation":{"files_verified":true,"id":3,"manifest_sha256":"97bcc77723139e1d84ac46f786ee85152e6be3ce0d35938d797c85b33429b983","matches_ledger":true,"verified_at_ns":1792061580401844000},"head":"34e64ba565ed353f7db1ab6f1e9611b8815053c45548c22b9827d2c3dfc415e5","intact":true,"problems":[]}
```

`intact` requires every check to pass. `problems` names each break by entry
— `seq 2: head does not match its contents (recorded 0dee…, recomputed
9edc…)` — or by file, prefixed with the generation. A ledgered erasure is
named by its entry until it is re-applied if the live tombstone log has no
settled erasure of its subject published by the entry's `generation` or
later. Restoring a pin from before the erasure does that, even when an
earlier erasure of the same subject is still settled. A settled erasure with
no entry of the same `erasure_id`, `subject_digest` and `generation` is
named as not in the ledger. A live generation newer than the latest
checkpoint entry is reported as not ledgered yet. A crash or a failed append
between publishing (or settling) and ledgering leaves either gap, and the
next open, or the next checkpoint after a failed append, ledgers what is
missing. An entry being appended while the check runs is not reported
missing. The file check reads every manifested byte under the maintenance lock,
so it costs O(store) like `GET /v1/verify`. The chain is read up to a snapshot
taken under that lock and walked after the lock is released, so an erasure or
anchor appended mid-check is left for the next one rather than read
half-written. This route always walks the whole chain and re-runs the file
check. Compaction and expiry
replace files between checkpoints, so until the next checkpoint the live
manifest can name files that are gone, reported as missing;
`POST /v1/checkpoint` first for a clean check.

The same verification runs offline, read-only, against a directory no live
server owns:

```sh
traza-server verify --ledger --data-dir /var/lib/traza [--json]
```

Exits `0` when intact, `2` when anything is broken (every problem printed),
`1` when it cannot verify at all — not a data directory, or owned by a live
server. It never opens the store, so a store that crashed between publishing
a generation (or settling an erasure) and ledgering it exits `2` with the
not-ledgered-yet (or not-in-the-ledger) problem until its next open ledgers
it.

### `POST /v1/ledger/anchors`

**Requires an unbound `admin` credential.** Records an external notary's
countersignature over an earlier head. The body is the anchor's fields; the
ledger assigns `seq`, `prev`, `head` and `at_ns`, and refuses a body that
names any other field.

```sh
curl -X POST http://localhost:8080/v1/ledger/anchors -d '{
  "target_seq":4,
  "target_head":"34e64ba565ed353f7db1ab6f1e9611b8815053c45548c22b9827d2c3dfc415e5",
  "authority":"notary.example","anchored_at_ns":1,
  "signature":"s","public_key":"k","note":"q3"}'
```

`201`, with the appended entry:

```json
{"anchored_at_ns":1,"at_ns":1792061622506982000,"authority":"notary.example","head":"d3e217800bca9aed7d05ba27d5fe88c954f4e2cc452b5c22ac192001f1547a61","kind":"anchor","note":"q3","prev":"34e64ba565ed353f7db1ab6f1e9611b8815053c45548c22b9827d2c3dfc415e5","public_key":"k","seq":5,"signature":"s","target_head":"34e64ba565ed353f7db1ab6f1e9611b8815053c45548c22b9827d2c3dfc415e5","target_seq":4}
```

`signature`, `public_key` and `authority` are stored opaque — Traza does not
know the scheme and does not check the signature. What it does check is that
`target_head` is the head recorded at `target_seq`.

**Every tenant sees anchors whole.** An anchor entry appears unredacted in
every tenant's evidence pack, `authority` and `note` included — it is what
lets a tenant's auditor check the chain against the notary. Keep anything
tenant-specific out of those fields.

Errors: `409` when `target_head` is not the recorded head — including one
that is not 64 lowercase hex characters, which cannot be — or when
`target_seq` does not exist yet
(`{"error":"the ledger has no entry 99 yet; its head is seq 6"}`); `400` for
an empty `authority`/`signature`/`public_key`, a field over 4096 bytes, or an
unknown field; `403` for an `rw` or bound credential.

### `GET /v1/evidence`

The evidence pack. Any `ro` credential; a bound credential always gets its
own tenant's pack.

| Parameter | Meaning |
|---|---|
| `since`, `until` | The window, Unix ns (`since_ns`/`until_ns` accepted); default the epoch to now |
| `tenant` | The tenant to cover (empty names the default tenant). Unbound only — omitted means the whole store; a bound credential naming another tenant gets `403` |

Abbreviated (heads and entries elided):

```json
{"activity":{"by_day":[{"cost_usd":0.00001,"day":"2023-11-14","errors":0,"spans":1},
                       {"cost_usd":1.5e+20,"day":"2023-11-15","errors":1,"spans":1}],
             "errors":1,"sessions":1,"spans":2,"traces":2},
 "digest":"a621ab3b040e67bd6f8899c3654dea99b4e42b5ad56e9259a701fad9837c6bf9",
 "engine_version":"0.27.0",
 "erasures":[{"conclusive":true,"id":1,"published_at_ns":1792061585528589000,"result":"erased","subject_kind":"session"}],
 "format":"traza-evidence/1","generated_at_ns":1792061592996325000,
 "ledger":{"entries":[…],"entries_truncated":false,"entries_unreadable":0,"head":{"at_ns":1792061585533710000,"head":"34e6…","seq":4,"store_id":"66022a6e1d87f73e3ba52202b53d1998"},
           "verification":{"entries":5,"generation":{…},"head":"…","intact":true,"problems":[]}},
 "retention":{"source":"none","ttl_seconds":null},
 "store_id":"66022a6e1d87f73e3ba52202b53d1998","tenant":null,"usage":null,
 "window":{"since_ns":0,"until_ns":1792061592996325000}}
```

`digest` is the SHA-256 of the canonical JSON of every other field — what a
notary signs. One `--query-deadline-ms` budget covers the whole pack. The
chain walk continues the previous pack's (or verification's) walk when that
walk began within the last 60 seconds, so a pack usually walks only what was
appended since. If the budget runs out mid-walk, the pack answers `400`
(`… the ledger chain was verified through line N …`); the walk keeps its
progress, so a retry resumes and completes it. `activity` counts spans that
started in the window and are still stored, from the same fold as the
analytics, under the same budget (a window too large for it answers `400`,
never a partial pack). `erasures` lists those settled in the window with
their receipt's `conclusive` and `result`; past 32 receipts or the budget
both are `null`. The file check, reused for 60 seconds, is the one step the
budget does not cut short. `ledger.entries` holds entries whose `at_ns` is in the window, at
most 10,000 (`entries_truncated` says whether more exist), and
`ledger.entries_unreadable` counts the chain's lines the pack could not place
(not a JSON object, or no integer `seq` or `at_ns`); the verification names
each. `ledger.head`, `ledger.entries` and `ledger.verification` come from one
snapshot of the chain. A tenant's pack carries that tenant's
[`GET /v1/usage`](#get-v1usage) row as `usage` and the retention window expiry
applies to it (`source`: `limits`, `flag`, `global` or `none`); a whole-store
pack has `"usage":null` and the global window. A tenant's pack shows
other tenants' erasure entries only as stubs (`seq`, `kind`, `at_ns`,
`prev`, `head`, `"redacted": true` — no `salt`, so the head cannot be
recomputed for a guessed subject), summarizes file problems as a count, and
withholds verification problems about other tenants' erasures behind a count;
anchors appear whole. The field table, the scoping rules, and a stdlib Python
check of the digest are in [the ledger guide](ledger.md#evidence-packs).

---

## Operations

### `GET /healthz` and `/readyz`

Liveness and readiness probes, answered **before** the auth gate — an
orchestrator probing them holds no credential — and not counted in the request
metrics. `GET` only; any other method goes through the gate like any request.

```json
{"status":"ok","version":"0.27.0"}
```

```json
{"ready":true}
```

`version` is the server's build version (the crate version it was built
from). The listener binds only after the
store has opened and recovered its log, so `/readyz` answers `200` whenever it
answers at all; "not ready yet" is a refused connection.

### `GET /v1/stats`

```json
{"buffer_age_seconds":4,"buffered_records":1,"bytes_on_disk":0,"durability":"wal","persisted_records":0,"record_count":1,"segment_count":0,"total_records":1,"wal_bytes":261}
```

| Field | Type | Description |
|---|---|---|
| `buffered_records` | integer | Primary-key-unique records in the in-memory write buffer |
| `persisted_records` | integer | **Physical** records in segments, including historical versions superseded by last-write-wins |
| `total_records` | integer | `buffered_records + persisted_records` |
| `record_count` | integer | Same value as `total_records` |
| `segment_count` | integer | Persisted segment files |
| `bytes_on_disk` | integer | Total size of segment files |
| `durability` | string | `buffered`, `wal`, or `flushed` |
| `wal_bytes` | integer | Bytes the write-ahead log holds — the work a restart would replay. Zero in `buffered` mode and immediately after a flush |
| `buffer_age_seconds` | integer or null | Seconds the oldest buffered span has waited for a seal; `null` when the buffer is empty. Climbing past `--max-buffer-age-seconds` means nothing is scheduling buffer maintenance |

`persisted_records` counts physical records so that the call stays
O(number of segments) instead of decoding the corpus. It is therefore an upper
bound on the number of distinct spans, not the count of them. See
[monitoring](../operations/monitoring.md#get-v1stats).

**Errors.** `503` on store failure.

### `GET /v1/metrics`

Prometheus text exposition format (`Content-Type: text/plain; version=0.0.4`).
Engine stages first, then the HTTP layer.

```
# TYPE traza_spans_admitted_total counter
traza_spans_admitted_total 4
…
```

Every metric name and its meaning is documented in
[monitoring](../operations/monitoring.md), including the accuracy bound on the
percentile gauges. Two families carry a `tenant` label, one series per tenant
with usage counters: `traza_tenant_spans_ingested_total` and
`traza_quota_rejected_total` (the [`/v1/usage`](#get-v1usage) lifetime
counters).
The run guard adds `traza_fuse_trips_total`, `traza_fuse_warnings_total` and
`traza_fuse_live_runs`, unlabelled (absent under `--no-fuses`).

**Errors.** None — the route always renders.

### `GET /v1/metrics.json`

The same numbers as [`/v1/metrics`](#get-v1metrics), shaped for a browser.
Prometheus text exposition is for scrapers; asking a dashboard to ship an
exposition parser in order to draw one chart is a parser nobody should have to
write.

```json
{"uptime_ns":535200000000000,"durability":"wal","requests":{"total":184204,"rejected":0,"responses_2xx":184204,"responses_4xx":0,"responses_5xx":0,"mean_ns":1210880,"max_ns":75821375,"p50_ns":1703935,"p95_ns":2818047,"p99_ns":9437183},"by_class":{"search":{"count":8,"mean_ns":22375885,"max_ns":75821375,"p50_ns":1703935,"p95_ns":2818047,"p99_ns":9437183},"lookup":{},"stats":{},"ingest":{},"other":{}},"connections":{"accepted":12,"refused":0,"live":1},"decode":{"spans":2417882,"mean_ns":91375,"p95_ns":131071},"ingest":{"spans_admitted":2417882,"batches_admitted":2418,"wal_commits":2418,"wal_fsync_p95_ns":9879551,"segment_seal_p95_ns":0},"pruning":{"segments_examined":340,"segments_pruned_by_time":328},"percentile_error_bound":0.0625}
```

Request latency is split by **route class** — `ingest`, `lookup`, `search`,
`stats`, `other` — because an ingest batch and a trace lookup differ by orders
of magnitude and one blended histogram described neither. `by_class` carries
`count`, `mean_ns`, `max_ns` and the three percentiles for each.

`durability` is the same value [`GET /v1/stats`](#get-v1stats) reports, carried
here so a client showing server health does not have to infer what an
acknowledged write guarantees — or, as the dashboard once did, default it.

`percentile_error_bound` is stated in the payload rather than left implicit:
every `p*_ns` here is a bucket upper bound, at most that fraction high and
never low. Counts, sums, means and maxima are exact.

**Errors.** None — the route always renders.

### `POST /v1/mcp`

The [Model Context Protocol](../guide/mcp.md) endpoint: JSON-RPC 2.0 over the
Streamable HTTP transport, one message per request. **Served only when the
server was started with `--mcp`**; otherwise every method answers `404` with
the flag named.

This route is documented in full in **[the MCP guide](mcp.md)** — its sixteen
tools, five resources, three templates and four prompts, and what each returns.
What belongs here is the transport contract:

| Condition | Response |
|---|---|
| A JSON-RPC request | `200` with one JSON-RPC response |
| A JSON-RPC notification or response | `202` with **no body** |
| `GET` or `DELETE` | `405` — no server-initiated SSE stream, no session to delete |
| `Origin` present, and neither loopback nor named by `--mcp-allowed-origin` | `403` — the transport's DNS-rebinding defence. The origin is never validated against the request's own `Host`, which a rebinding request also controls |
| `MCP-Protocol-Version` naming an unserved revision | `400`, listing the supported ones |
| A body that is not JSON | `400` with JSON-RPC `-32700` |
| A JSON array (a batch) | `400` with JSON-RPC `-32600` — batching was removed from MCP |

Supported protocol revisions are `2025-11-25` and `2025-06-18`. A revision this
server serves is echoed back from `initialize` unchanged; anything else is
answered with `2025-11-25` and the client decides whether to continue.

**Authorization is per tool, not per method.** Unlike every other route here,
the HTTP method does not describe the operation: MCP tunnels reads and writes
alike through one `POST`. A `ro` token therefore reaches every read tool, and
`record_annotation` additionally requires `rw` *and* `--mcp-annotations`.

```sh
curl -X POST http://localhost:8080/v1/mcp \
  -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

### `GET /` and `/dashboard`

Serve the built dashboard's `index.html`; other paths under the build root
serve their assets with the right content type. `/dashboard/` works too.

These routes are served **before** the authentication gate: the page is static
build output carrying no data, and every `/v1` call it makes stays gated. Path
traversal outside the build root is refused.

With no dashboard build available:

```json
{"error":"no dashboard build found","next":"build it with: cd ui && npm ci && npm run build (serving /path/to/ui/dist)"}
```

with status `404`. The API is unaffected.
