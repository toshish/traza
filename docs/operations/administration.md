# Administration

Authentication, retention, compaction, and payload offloading — the things you
configure once and then live with. Flag syntax and defaults are in the
[configuration reference](../configuration.md); this page explains the
behaviour behind them.

## Authentication

Traza is unauthenticated by default **on loopback only**. Credentials come from
two places, and they coexist:

- **`TRAZA_TOKENS`** — configured in the environment, fixed for the life of the
  process.
- **Runtime credentials** — minted, listed and revoked over HTTP by an unbound
  `admin` while the server runs; see [Runtime credentials](#runtime-credentials).

With neither, the server is open (on loopback). **The moment any credential
exists, from either source, every request must authenticate.**

```sh
TRAZA_TOKENS="rw:$(openssl rand -hex 16),ro:$(openssl rand -hex 16)" \
  traza-server --data-dir /var/lib/traza --host 0.0.0.0
```

### The token format

A comma-separated list of `scope[@tenant]:token` entries.

| Scope | Permits |
|---|---|
| `ro` | `GET` only |
| `rw` | `GET` and `POST` |
| `admin` | Everything `rw` permits, **plus erasure**, eval version tombstones, and — unbound only — backups and the `/v1/admin/*` routes |

Tokens must be non-empty, unique, and free of whitespace and commas. Entries
may not have surrounding whitespace. The first `:` isolates the token — which
may itself contain `@` or `:` — and only the left side is examined for a
binding, which is what makes the syntax unambiguous.

**The optional `@tenant` binds the credential to one tenant:**

```sh
TRAZA_TOKENS="rw@acme:$(openssl rand -hex 16),admin:$(openssl rand -hex 16)"
```

`rw@acme:…` writes and reads tenant `acme` and nothing else. Operationally, a
bound credential is the token you hand a customer's collector or dashboard in
a multi-tenant deployment:

- **Ingest is stamped and checked.** Spans that name no tenant get the
  binding; a span claiming a different tenant fails its whole batch with
  `400` — loudly, because silently rewriting it would hide a misconfigured
  exporter forever.
- **Every read is scoped.** The binding is applied where queries are
  constructed — one choke point, so no endpoint can forget it — and naming a
  foreign tenant in any `tenant=` parameter is a
  `403 {"error":"this credential is bound to a different tenant"}`.
- **The store-global operator endpoints refuse bound tokens.** `/v1/stats`,
  `/v1/metrics`, `/v1/metrics.json`, `/v1/verify`, `/v1/checkpoint`,
  `/v1/flush`, and `/v1/backups/*` answer bound credentials `403`: they
  report and move the whole store's data, and even the volumes disclose
  co-tenants. A bound credential's accounting surfaces are
  [`GET /v1/usage`](#quotas-and-usage) and
  [`GET /v1/tenants`](#tenant-accounting).
- **Everything under `/v1/admin/` is unbound-admin only**, by prefix: a bound
  credential is refused on every path there, including paths that do not
  exist yet, so a route added under the prefix later is denied before anyone
  remembers to list it.
- **Cross-tenant reads answer `404`, never `403`** — existence is a fact
  about another tenant, and a distinguishable refusal would leak it.
- A bound `admin` may erase only its own tenant; see
  [Erasure](#erasure-deletion-with-a-receipt).

**An invalid `TRAZA_TOKENS` refuses startup.** Not a warning, not a fallback to
open — the process exits. Silently running open when the operator tried to
configure authentication would be the worst possible failure mode. The error
names the defect (`TRAZA_TOKENS contains an invalid scope`,
`TRAZA_TOKENS contains an invalid tenant binding`) and never echoes the
offending value. Bindings must satisfy the tenant charset ingest enforces —
lowercase `[a-z0-9][a-z0-9._-]`, at most 64 bytes — so a credential can never
be bound to a tenant no span could ever carry.

### Behaviour

| Situation | Response |
|---|---|
| No, malformed, unknown, expired, or revoked token | `401 {"error":"unauthorized"}` with `WWW-Authenticate: Bearer` — the same body for all five, so a leaked token's state is not disclosed to whoever holds it |
| `ro` token on a `POST` | `403 {"error":"forbidden","required":"rw"}` |
| Non-`admin` token on erasure, a tombstone, a backup, or `/v1/admin/*` | `403 {"error":"forbidden","required":"admin"}` |
| Bound token on an operator endpoint or `/v1/admin/*` | `403 {"error":"forbidden","required":"unbound"}` |
| Open mode: a browser request from a non-loopback page to erasure, a tombstone, a backup, or `/v1/admin/*` | `403 {"error":"origin not allowed: …","next":"…"}` — see [Bootstrapping](#runtime-credentials) |
| A `/v1/admin/*` `POST` whose body is not declared `Content-Type: application/json` | `415` |
| Valid token, permitted request | The route's normal response |

`"required"` names the capability the credential lacked, so a dashboard can
say which grant would have worked rather than only that this one did not.

**Lookup is by digest.** Every credential, environment ones included, is held
as the SHA-256 of its token; the presented bearer is hashed and the digest
looked up in a table. The lookup's timing depends on the digest, not the
token, and learning bytes of a SHA-256 output does not help anyone construct a
token that produces it. `AuthConfig` (the `TRAZA_TOKENS` parser) implements a
redacted `Debug`, so an accidental structured log cannot disclose credentials.

The verdict is reached from the request **head**, before the body is read: an
unauthenticated client cannot make the server buffer a 64 MiB body just by
declaring one. A rejected request closes its connection, precisely because that
body was never consumed.

### The one route that authorizes per operation, not per method

`POST /v1/mcp` is the exception, and it is deliberate. The `ro`/`rw` rule above
maps scope to HTTP method because everywhere else the method *is* the
operation. The [MCP endpoint](../guide/mcp.md) tunnels reads and writes alike
through one `POST`, so applying the method rule there would either refuse every
`ro` token a read-only surface or hand every caller that got in the write
scope.

The token is authenticated identically — same digest lookup, same 401 — and
then authorized per tool: a `ro` token reaches every read tool, and
each writing tool additionally requires `rw` **and** its own switch. A tool the
presented token cannot call is not advertised to it.

| Tool | Needs |
|---|---|
| `record_annotation` | `rw` + `--mcp-annotations` |
| `promote_failures_to_dataset` | `rw` + `--mcp-promote` |

**The two switches are separate on purpose, and `--mcp-promote` is the larger
grant.** An annotation is a fact recorded beside a span, and the erasure that
removes the span removes it. A promoted example is a *copy* that deliberately
outlives its source — that is what makes a dataset useful as a regression
suite — so erasing the source trace afterwards does not remove it, and the
payload references it carries keep those bytes alive past their retention
window. Deleting it means tombstoning the dataset version, or erasing the whole
tenant. Grant it to an agent you would let keep a copy.

### Runtime credentials

An unbound `admin` manages credentials over HTTP while the server runs —
`POST /v1/admin/credentials` to mint (or register) one,
`GET /v1/admin/credentials` to list them, and
`POST /v1/admin/credentials/{id}/revoke` to revoke one. Shapes are in the
[HTTP API](../guide/http-api.md#post-v1admincredentials).

```sh
curl -X POST http://localhost:8080/v1/admin/credentials \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"scope":"rw","tenant":"acme","label":"acme collector"}'
```

Admin request bodies must be declared `application/json`; anything else is
`415`.

- **Bootstrapping.** On a loopback start with no credential anywhere, the
  server is open and the caller is an unbound admin, so the first credential
  can be minted with a plain `curl` on the host:

  ```sh
  curl -X POST http://localhost:8080/v1/admin/credentials \
    -H 'Content-Type: application/json' -d '{"scope":"admin","label":"root"}'
  ```

  **The first credential must be an unbound `admin`** — anything else is
  `409`, because it would close the store with no credential able to reach
  `/v1/admin/*`. From that request on, the store is closed — and the
  credential log persists, so it stays closed across restarts, and a later
  non-loopback start counts the log as configured authentication.
- **No bootstrapping from a web page.** While the store is open, erasure,
  tombstones, backups and `/v1/admin/*` refuse a browser request whose
  `Origin` is not a loopback page (`403`). Otherwise any site the operator
  visited could mint itself an admin credential with a cross-site `POST` —
  locking the operator out and keeping a token that a later non-loopback
  start would serve to the network. `curl` sends no `Origin` and the
  dashboard is a loopback page, so neither is affected. Once a credential
  exists the rule is not applied: a page cannot attach a bearer token it does
  not know.
- **The token is shown once.** It is in the `201` response and nowhere else:
  the store writes only its SHA-256. A minted token is `trz_` plus 64 hex
  digits from `/dev/urandom`. The list shows a short `prefix` — at most a
  quarter of the token, never more than 12 characters — to tell credentials
  apart.
- **Registering a token.** A control plane that issues tokens itself may pass
  `"token"` (at least 24 bytes, no whitespace or commas) to register it.
  **Supply a high-entropy token** — at least 128 random bits, such as
  `openssl rand -hex 16` or longer. Only the length is checked, and at rest
  the token is protected by nothing but an unsalted SHA-256, which a guessable
  token does not survive. A token's digest can be registered once, ever: a
  duplicate — including a revoked token, or one in `TRAZA_TOKENS` — is `409`,
  so a revoked token can never come back.
- **Revocation is immediate and durable.** The revoke record is fsynced before
  the response; from the next request the token is `401`, and a `kill -9`
  does not bring it back. Environment credentials cannot be revoked this way
  (`409`): they are revoked by restarting without them.
- **Expiry.** `expires_in_seconds` gives a credential a lifetime; at its
  instant it starts answering `401`, like a revoked one, and stays listed.
- **Revoking every credential locks the store; it does not reopen it.** Once
  any runtime credential has been created, enforcement stays on. If every
  credential is revoked, start the server with `TRAZA_TOKENS` to get back in.
- `last_used_at_ns` is kept in memory only, so a restart forgets it — writing
  it durably would put an fsync on every authenticated request.

**Storage.** `credentials.jsonl` in the data directory: append-only `create`
and `revoke` records holding digests, never tokens, fsynced per write; a torn
final record (a crash mid-append) is truncated at the next start, and damage
anywhere else refuses the start rather than guessing which credentials the
file held. **It is outside generations and pins**: a backup never carries it,
and `--restore` leaves the live file in place — restoring last month's backup
does not resurrect a token revoked last week. The file is created owner-only
(`0600`). Back it up separately if you need to, knowing it holds digests: for
minted tokens (256 random bits) those authenticate nobody without the tokens,
but a weak supplied token could be guessed from its digest offline.

Creating or revoking a credential fsyncs the log while holding a writer lock
of its own; requests keep authenticating through it, and a revoked token
stops working once the revoke record is durable, before the response is
sent.

### The non-loopback refusal

With no `TRAZA_TOKENS` and no runtime credential in the data directory, a
non-loopback `--host` is refused at startup:

```
traza-server: refusing unauthenticated non-loopback bind 0.0.0.0; configure TRAZA_TOKENS, create a runtime credential (POST /v1/admin/credentials on a loopback start), or pass --allow-unauthenticated-non-loopback explicitly
```

`--allow-unauthenticated-non-loopback` is the deliberate escape hatch — it
exists so that running open on a network is a decision someone made, not
something that happened by default.

**Running open on a network hands the store to whoever reaches it first.**
Every client that can reach the port is an unbound admin: it can read, write
and erase everything, and the first to call `POST /v1/admin/credentials` mints
the store's first admin credential. That closes the store to everyone else at
once, and it persists: the credential log survives restarts, and a restart
with `TRAZA_TOKENS` adds the environment's credentials beside it rather than
removing it — removing a planted credential means revoking it with an admin
token, or moving `credentials.jsonl` aside while the server is stopped. The
cross-site check above does not help here: it stops browsers, not network
clients, which send no `Origin`. Configure a credential instead wherever the
network is not entirely trusted.

### What is *not* gated

The dashboard **shell** (`/`, `/dashboard`, and its static assets) is served
before the auth gate. It is build output carrying no data, and it must load in
a browser without credentials so it can prompt for a token. Every `/v1` call
the page makes is gated exactly like any other client's. The page holds the
token in `sessionStorage` only.

`GET /healthz` and `GET /readyz` are answered before the gate too, so a load
balancer or orchestrator can probe without a credential; they disclose
nothing but the build version.

### Operational notes

- **TLS is reverse-proxy territory.** Traza speaks plain HTTP; tokens on an
  unencrypted network are readable.
- **Supply tokens through the environment, not the command line.** Arguments
  are visible in `ps`. A systemd `EnvironmentFile` with mode 0600 is the
  straightforward answer.
- **Rotating a runtime credential is not a restart.** Mint the new one, move
  clients over, revoke the old one — every step takes effect on the next
  request. Rotating a `TRAZA_TOKENS` entry still is a restart (there is no
  reload signal): configure the new token alongside the old, restart, move
  clients over, then remove the old one and restart again. To move off
  environment tokens entirely, mint runtime replacements, move clients, and
  restart without `TRAZA_TOKENS`.
- Give collectors and exporters an `rw` token; give dashboards, alerting, and
  anything read-only a `ro` token. A `ro` token cannot ingest, flush, or
  annotate — and over MCP it reaches the read tools but not the writer.
- Mint `admin` tokens for operators and compliance tooling only, and never
  hand one to a telemetry producer. Erasure is the one destructive verb in
  the API, and it is gated on this scope precisely so that a compromised or
  overprivileged ingest credential cannot destroy the telemetry it writes. An
  unbound `admin` can also mint credentials, so treat it as the root of the
  deployment.
- **Two similarly named variables.** `TRAZA_TOKENS` (plural) configures the
  *server's* credential set. `TRAZA_TOKEN` (singular) is the bearer token the
  bundled `seed --url` client sends. They are not interchangeable.

## Retention (TTL)

Off by default — **nothing is ever deleted unless you ask.**

```sh
traza-server --data-dir /var/lib/traza --ttl-seconds 604800   # 7 days
```

A background pass runs **every minute** and removes:

- **Spans whose `end_time_ns` is before the cutoff.** Retention is by the span's
  own end timestamp, not by ingest time — a span backdated at ingest is expired
  on its own clock.
- **Annotations older than the cutoff**, by their own timestamps.
- **Payload files older than the cutoff**, by mtime, excluding those still
  referenced. Live references are computed *after* span expiry, so a payload
  referenced only by just-expired spans becomes sweepable in the same pass. An
  orphan lingers at most one TTL past its span.

`--ttl-seconds 0` means **disabled**, not "expire everything now".

### Per-tenant retention

`--tenant-ttl TENANT=SECONDS` (repeatable) overrides the window for one
tenant:

```sh
traza-server --data-dir /var/lib/traza \
  --ttl-seconds 604800 \
  --tenant-ttl acme=2592000 --tenant-ttl trial-co=86400
```

A tenant's cutoff is resolved in one order: **its runtime limits document's
`ttl_seconds`, else its `--tenant-ttl` override, else `--ttl-seconds`, else
never.** A tenant with no override on a server with no global TTL keeps
everything, whatever other tenants are configured with.

The runtime layer is set with
[`POST /v1/admin/tenants/{tenant}/limits`](../guide/http-api.md#post-v1admintenantstenantlimits)
(`"ttl_seconds": 0` keeps everything, `null` falls back to the flags) and takes
effect at the next minutely pass, with no restart — the retention pass now
runs every minute whether or not any flag configured a window, and returns
immediately when nothing does.
Retention is a per-tenant policy the moment tenants exist — a single window
forced on every tenant would make one customer's compliance clock another's
data loss. `--tenant-ttl acme=0` disables `acme`'s window (same rule as the
global flag: zero is "disabled", never "expire everything now"), and an
invalid tenant name refuses startup.

Two consequences worth knowing:

- **The retire-whole fast path only runs when a global TTL covers everyone.**
  Deleting a segment outright because every span in it is expired is sound
  only against a bound *every* span is subject to; with per-tenant overrides
  and no `--ttl-seconds`, a tenant with no window has no cutoff at all, and
  retiring a segment on the other tenants' clocks would delete that tenant's
  unexpired spans with it. Such segments are decoded and rewritten to their
  survivors instead — correct, just not free.
- **Scores are exempt from TTL entirely.** A score — an annotation addressed
  to an experiment example — lives on eval retention, not trace retention: a
  rolling window that swept January's scores would silently empty the base of
  every experiment-over-experiment diff run in March. Other annotations age
  out on their own tenant's window.

Expiry rewrites the segments it touches with the same
write-temp-fsync-rename discipline as any other rewrite — onto the same file
name, so an expired segment keeps its place in recency order — and an
interrupted pass leaves either the original or the survivors, never both.

**Expiry deletes, it does not merely hide.** A span still in the write buffer
lives in the write-ahead log too, so expiry rewrites the log to exactly the
spans that survived. The expired bytes leave the disk in that pass rather than
being marked dead, and a restart cannot bring the span back. This matters
twice: retention that a restart undoes is not retention, and if you are
deleting telemetry because someone asked you to, the log is one of the places
it has to leave.

Retention runs with reads and ingest fully live. Only one rewriting pass —
expiry or compaction — runs at a time.

## Erasure (deletion with a receipt)

TTL answers "how long do we keep telemetry"; erasure answers "this specific
data must go, and prove it". `POST /v1/erasures` erases a **subject** — a
trace, a span, a session, an offloaded payload, or a whole tenant — from
every domain, and
`verify --erasure` (or `GET /v1/erasures/{id}/verify`) produces the receipt:
every place the subject's bytes could be, checked by name, with the result of
each. Endpoint shapes are in the
[HTTP API](../guide/http-api.md#erasure); this section is the operational
contract.

**Erasure requires the `admin` scope.** When `TRAZA_TOKENS` is configured,
`POST /v1/erasures` refuses `rw` with a `403`: every collector holds a write
token, and a credential minted to produce telemetry must not be the
credential that destroys it. Reading the tombstone log and the receipt stays
`ro` — identifiers, never content. On a loopback-open server (no tokens),
erasure is open with everything else.

**Subjects are tenant-scoped.** Trace, span, and session subjects carry a
`tenant` field; empty is the default tenant, never "all tenants" — two
tenants sharing a trace id are two subjects, and erasing one leaves the other
untouched. The `tenant` subject kind erases everything one tenant owns —
spans, annotations and scores, datasets, versions, examples, experiments, and
reference-aware deletion of the payload bytes they held — and requires a
non-empty name: the default tenant is every store that never configured
tenancy, and "erase it whole" is "erase the store", which is not an API.
Tenant subjects record no span keys (a tenant's key set is unbounded; the
mask and purge cover by predicate), so for a whole tenant **the settle time
is the re-delivery line**. While one is pending, eval mutations for that
tenant answer `409` rather than racing the purge.

**A bound `admin` erases only its own tenant.** Naming a foreign tenant —
tenant subjects included, or a per-tenant admin could destroy a neighbour
wholesale — is a `403`. Payload subjects carry no tenant (content addressing
is store-global), so they are an unbound-operator act by rule: a bound admin
gets `403 {"error":"payload subjects are store-global; erasing one requires
an unbound admin credential"}`. Bound credentials also see only their own
tenant's records in the tombstone-log listings.

**What an erasure does.** The intent is fsynced into `tombstones.jsonl`
before anything is removed — from that moment the subject is invisible to
every query, **and covered spans are dropped at ingest**: the pending
erasure is an admission barrier, so a client replaying covered data while
the erasure runs gets an acknowledgement and no storage (counted in
`traza_erasure_spans_suppressed_total`). A crash mid-purge leaves a pending
erasure the next open masks and the maintenance tick finishes. The purge then
rewrites the write buffer and the write-ahead log to the survivors, rewrites
every segment holding a match in place (superseded versions of an erased key
held the bytes too, so they go with it), drops annotations addressed to
erased spans, and deletes payload files **reference-aware**: content
addressing means one file can back spans outside the subject, and those
bytes are retained and named in the receipt rather than destroyed. The
barrier is total while the erasure is pending: covered spans are dropped
**before payload offloading** (a suppressed span must not leave orphan
payload bytes behind), oversized values whose content hash IS the subject
offload directly to their redacted marker instead of recreating the file,
and covered annotations are dropped at admission too. Every rewrite —
including a confirm pass for anything in flight when the barrier went up —
happens **before** the checkpoint, so the generation the settle record cites
digests exactly the store the erasure left behind and verifies clean
afterwards. Then the checkpoint publishes the deletion — durable at the
`CURRENT` rename — and the settle record lands, lifting the barrier. Nothing
can be acknowledged before `settled_unix_ns` and survive it.

**What remains, on purpose.** The tombstone log keeps the subject's
identifiers, the resolved span keys, and the payload content hashes — never
the erased text. That record is what verification checks the store against,
and the receipt states its retention rather than hiding it. Erasing the
record of erasure means deleting the store. One caveat the settle record
carries in its own docs: its counts are the settling pass's counts — after a
crash-resume the physical work of the first pass is already done and cannot
be re-counted, so the audit-grade answer to "is it gone" is always the
receipt, never the tallies.

The [ledger](../guide/ledger.md) keeps a second record. Every settled erasure
appends one entry to `ledger.jsonl`: the erasure id, the subject kind, the
tenant (`null` for a payload subject), the generation that published it, and
a SHA-256 of the subject — never a trace, span or session id or a payload
reference. For a **tenant subject the `tenant` field is the erased tenant's
name**, so the entry identifies it outright. The ledger never rotates, every
backup pin carries a copy of it, and rewriting an entry breaks the chain that
makes it evidence — so this record outlives the erasure on purpose too, and
the receipt's `ledger` domain says so.

**Pins hold their bytes.** A backup pinned before the erasure still contains
the subject in its hard-link farm — that is what a pin is for. The receipt
checks every pin and names the ones to release; a backup already copied
elsewhere is outside the data directory and outside the receipt's scope, and
the receipt says exactly that by only ever naming what it checked. The
converse also holds: **an erasure never edits a pin.** The append-only logs
are *copied* into a pin at their manifested length rather than hard-linked,
precisely so a later erasure's records cannot leak into a backup that still
holds the pre-erasure state.

**A tombstone is a barrier, not a ban.** Data ingested under the same
identifiers after the erasure settles is new data. The receipt tells the two
apart exactly — an erased key found live again is a **re-delivery** (some
client replayed erased data; the receipt fails), a fresh key under the same
trace or session id is **new activity** (reported, never a failure). If a
client with a retry queue may replay erased batches, drain it before erasing,
or expect the receipt to name the re-delivery and re-run the erasure.

**The receipt checks eval records too.** Between the annotations and
payloads domains sits `eval-records`, a decode-walk scoped to the subject's
tenant — never a raw byte scan of the shared log, so one tenant's receipt can
never name another tenant's datasets. What it reports depends on the subject,
and the two non-clear results mean different things:

- For a **trace/span/session** subject, examples whose provenance or content
  is traceable to the subject report as **`attention`** and leave the receipt
  inconclusive: a promotion **copy** survives source erasure by design, and
  purging it is a deliberate second act (tombstone the dataset version, erase
  the payload) — the receipt names the copies so the operator can decide,
  rather than pretending the erasure reached data it was never asked to
  reach.
- For a **payload** subject, examples still carrying the reference report as
  **`retained-by-design`** and the receipt stays conclusive: after the blob's
  deletion the reference is a dangling **address, not content** — the bytes
  are gone, the version's digests remain valid, and purging the addresses is
  a version tombstone plus a future compaction.
- For a **tenant** subject, the domain re-counts the records the tenant still
  owns, which must be zero. The settle record carries the purge's tally as
  `eval_records_removed` (a tenant's scores are annotations and count under
  `annotations_removed`).

**`erased` and `conclusive` are separate answers.** The receipt's `result`
(`erased` or `not-erased`) is the semantic verdict on the erasure; its
`conclusive` flag describes the verification, and is false whenever an
over-approximate check — the byte-level occurrence scans over the log and
the rollup sidecars — found the subject's identifiers without proving them
benign, and whenever any domain could only reach `attention`. They disagree
in both directions: a receipt can be erased but inconclusive, and it can be
conclusively `not-erased` — a pinned backup definitively holds the bytes,
nothing ambiguous about it. The subcommand's exit code carries the
distinction: `0` erased and conclusive, `3` erased but inconclusive (read
the attention domains), `2` not erased.

**The MCP endpoint cannot erase.** Deletion is an HTTP-only verb behind the
`admin` scope. The agent-facing surface stays read-only by construction, so
stored adversarial text has no destructive tool to actuate.

## Tenant accounting

`GET /v1/tenants` answers "what does each tenant hold right now": logical
spans and distinct traces currently visible, approximate inline bytes,
approximate offloaded-payload bytes (a blob shared across tenants counts for
every referencing tenant — for a quota question, each of them is holding the
store to those bytes), and the activity window. A bound credential gets
exactly its own row. Shapes are in the
[HTTP API](../guide/http-api.md#get-v1tenants).

**It is an exact fold and it is O(store).** The route decodes the visible
spans to count them, on demand, against one pinned snapshot. That is the
right cost for an accounting question asked occasionally and the wrong one
for a dashboard polling per minute against a hundred-million-span corpus —
use `/v1/metrics` for continuous monitoring and this route when the per-tenant
answer is actually needed.

## Quotas and usage

Each tenant can carry a **limits document**, replaced whole at runtime by an
unbound admin with
[`POST /v1/admin/tenants/{tenant}/limits`](../guide/http-api.md#post-v1admintenantstenantlimits)
(`_default` names the default tenant):

```sh
curl -X POST http://localhost:8080/v1/admin/tenants/acme/limits \
  -H "Authorization: Bearer $ADMIN_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"monthly_spans":1000000,"monthly_bytes":10000000000,"ingest_paused":false,"ttl_seconds":null}'
```

- `monthly_spans` / `monthly_bytes` — what the tenant may ingest per UTC
  calendar month.
- `ingest_paused` — refuse every batch for the tenant, at once.
- `ttl_seconds` — the tenant's retention; see
  [Per-tenant retention](#per-tenant-retention).

**Usage counters.** The store counts every acknowledged batch per tenant —
lifetime spans, bytes, batches and refused spans, and the current month's
spans and bytes — and serves them in O(tenants) at
[`GET /v1/usage`](../guide/http-api.md#get-v1usage), a bound credential seeing
its own row. `bytes` is each span's native JSON after tenant stamping and
**before** payload offload: what the client sent, not what the store keeps.
The counters are written to `usage.json` every five seconds, by a server
thread of their own so a long merge or expiry pass cannot delay them, and on
every flush, when they changed; a crash loses the counts since the last
completed write — normally five seconds' worth, plus however long that write
took — an **undercount**, never an overcount.

**Enforcement.** Before a batch reaches the write-ahead log, a paused tenant's
batch is refused, and so is a batch whose spans or bytes would take the month
past a limit — the whole batch, never a prefix — with `429` and `Retry-After`
(the seconds to next month, or `60` for a pause). The body names the limit,
the month's usage and when it resets; see
[quota refusals](../guide/http-api.md#quota-refusals). Reads, exports and
erasure are never blocked, so a paused or capped tenant can always get its data
out. The check reads the counters as they stand without reserving anything:
batches admitted concurrently can together overshoot a limit by at most what
was in flight, after which the next batch is refused.

**What the limits do not do.** They meter ingest, not storage: deleting data
does not hand quota back (that is what [`GET /v1/tenants`](#tenant-accounting)
measures), and there is no rate limit within a month. The month is the UTC
calendar month; there is no per-tenant billing anchor.

**Erasure leaves them.** A tenant [erasure](#erasure-deletion-with-a-receipt)
removes the tenant's telemetry, not its accounting and access records: the
usage counters, the limits document, and any credentials bound to the tenant
survive it, and the erasure receipt does not examine those files. Revoke the
credentials and replace the limits document separately when a tenant leaves.

`limits.json` and `usage.json` live in the data directory **outside
generations and pins**, like `credentials.jsonl`: a backup carries telemetry,
not commercial terms or billing counters, and a restore rewinds neither. Both
are rewritten atomically (temp file, fsync, rename); a file that does not
parse refuses the start rather than running with no quotas — move it aside to
start from none.

## Compaction

On by default, and you should leave it on.

**Why it exists.** A filtered query narrows candidates through *every*
segment's index, so its cost tracks the **number of segments**, not the size of
the corpus. A store that only ever appends flush-sized segments accumulates
them without bound and gets steadily slower to search. Compaction merges
segments of similar size into larger ones to bound that count.

The measured effect is large — see [capacity](capacity.md#filtered-search-and-compaction).

**Two knobs.**

- `--compaction-fanout N` (default 4) — how many same-tier segments trigger a
  merge. Larger merges less often but leaves more segments to search. `0` or
  `1` cannot merge anything and are treated as "off" rather than looping.
- `--compaction-max-segment-bytes N` (default 268435456, i.e. 256 MiB) — the
  ceiling on a merged segment. This bounds the memory a merge needs, since it
  materializes its inputs; the cost is a floor on how far the segment count can
  fall. Raising it improves filtered search and costs memory. `0` removes the
  ceiling.

**A merge does not block the server.** Its inputs are pinned, every byte of
parsing, merging and fsyncing happens with no engine lock held, and only the
swap that publishes the result takes one — briefly, and after re-checking that
the inputs are still what it pinned. Reads and ingest therefore never wait on
the merge, which makes `--compaction-max-segment-bytes` a memory dial rather
than a stall dial. They do still share CPU and disk with it: a merge is real
work, and the effect of that contention on concurrent read and write latency is
**not measured** — see [capacity](capacity.md).

Merges run in the same maintenance thread as TTL, on a 5-second tick, and are a
no-op when no run qualifies.

**Correctness constraint, in case you are tempted to change it.** Only the
**tail** of the segment list is ever merged, because segment path order is
recency order and a merged segment takes a fresh (newest) id. Merging a run
from the middle would promote its spans past segments that legitimately
supersede them. See
[invariants](../internals/invariants.md#1-segment-path-order-is-recency-order).

**Crash safety** reuses the supersede journal: one marker per input, written
before the replacement exists, and the merged segment is renamed into place
before any input is deleted. No window drops data.

**If you disable compaction**, remember that every segment holds an open file
descriptor. A measured ~10,100 segments at 100M spans would exhaust a default
1024-fd limit, which is a second reason not to run large stores uncompacted.

## Payload offloading

String attribute values longer than `--payload-threshold-bytes` (default
262144, i.e. 256 KiB) are extracted at ingest into a content-addressed store
and replaced in the span by a reference:

```json
{"$payload": "sha256/29927e…", "bytes": 300000, "preview": "first characters…"}
```

The bytes live at `payloads/<first two hex chars>/<sha256>.bin` under the data
directory and are served by
[`GET /v1/payloads/sha256/{hex}`](../guide/http-api.md#get-v1payloadsreference).

**Why it exists.** Multi-megabyte prompts and completions do not belong inside
segment records: they bloat every decode that touches the span, and they would
bloat the attribute index. Offloading keeps them queryable without that cost.

**Content addressing dedupes.** An agent's system prompt repeated across ten
thousand calls is stored once. Files are immutable once written.

`--payload-threshold-bytes 0` disables offloading entirely, and oversized values
are then stored inline.

The threshold is a size/latency trade rather than a correctness one: lowering
it moves more content out of segments (smaller, faster decodes; more small
files) and raising it does the opposite. Payload files are swept on the TTL
window, so offloading interacts with retention as described above.

### Payload fetch across tenants

Content addressing is **store-global** — identical bytes are one file,
whoever ingested them — and that has a consequence for `GET /v1/payloads`
worth stating frankly rather than discovering.

For **unbound** deployments the route is capability-addressed: the full
SHA-256 is the capability, and knowing it means having read a span that
disclosed it. That argument is sound within one trust domain and it is also,
across tenants, a **cross-tenant existence oracle for guessable content**:
the hash of a suspected document is computable, and a `200` would confirm
some tenant stored those exact bytes. An unbound token is an operator
credential, so for it this is accepted.

**Bound credentials close the oracle.** A tenant-bound fetch must also prove
**reachability** — some span or dataset example of its own tenant carries the
reference (an example legitimately outlives its source span, so its holder
keeps fetch access) — and an unreachable payload answers `404` exactly as an
absent one does, indistinguishably. This is the deliberate M4 stance: the
boundary that matters is the tenant boundary, bound credentials are the
untrusted principals, and they are the ones the proof is demanded of. If
co-tenants must not be able to probe each other, do not hand tenants unbound
tokens.

## Backups

See [durability § Backups](durability.md#backups) — backup correctness is a
consequence of the crash-recovery model, so the two are documented together.

Two things specific to administration: taking or releasing a pin
(`/v1/backups/*`) needs an **unbound `admin`** credential, because a pin holds
its generation's bytes on disk until released; and a backup deliberately does
**not** contain `credentials.jsonl`, `limits.json` or `usage.json`, which a
restore leaves exactly as they are.

## See also

- [Deployment](deployment.md) — process topology, disk layout, service setup
- [Monitoring](monitoring.md) — what to watch and what to alert on
- [Configuration reference](../configuration.md) — every flag and its default
