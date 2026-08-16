# Administration

Authentication, retention, compaction, and payload offloading — the things you
configure once and then live with. Flag syntax and defaults are in the
[configuration reference](../configuration.md); this page explains the
behaviour behind them.

## Authentication

Traza is unauthenticated by default **on loopback only**. Set `TRAZA_TOKENS` to
require bearer tokens.

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
| `admin` | Everything `rw` permits, **plus erasure** |

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
  co-tenants. A bound credential's accounting surface is
  [`GET /v1/tenants`](#tenant-accounting).
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
| No, malformed, or unknown token | `401 {"error":"unauthorized"}` with `WWW-Authenticate: Bearer` |
| Valid token, wrong scope for the method | `403 {"error":"forbidden"}` |
| Valid token, permitted method | The route's normal response |

Comparison is constant-time, and every configured credential is checked even
after a match, so lookup timing does not depend on credential ordering.
`AuthConfig` implements a redacted `Debug`, so an accidental structured log
cannot disclose credentials.

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

The token is authenticated identically — same constant-time comparison, same
401 — and then authorized per tool: a `ro` token reaches every read tool, and
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

### The non-loopback refusal

Without `TRAZA_TOKENS`, a non-loopback `--host` is refused at startup:

```
traza-server: refusing unauthenticated non-loopback bind 0.0.0.0; configure TRAZA_TOKENS or pass --allow-unauthenticated-non-loopback explicitly
```

`--allow-unauthenticated-non-loopback` is the deliberate escape hatch — it
exists so that running open on a network is a decision someone made, not
something that happened by default.

### What is *not* gated

The dashboard **shell** (`/`, `/dashboard`, and its static assets) is served
before the auth gate. It is build output carrying no data, and it must load in
a browser without credentials so it can prompt for a token. Every `/v1` call
the page makes is gated exactly like any other client's. The page holds the
token in `sessionStorage` only.

### Operational notes

- **TLS is reverse-proxy territory.** Traza speaks plain HTTP; tokens on an
  unencrypted network are readable.
- **Supply tokens through the environment, not the command line.** Arguments
  are visible in `ps`. A systemd `EnvironmentFile` with mode 0600 is the
  straightforward answer.
- **Rotation is a restart.** There is no reload signal. Configure the new token
  alongside the old, restart, move clients over, then remove the old one and
  restart again.
- Give collectors and exporters an `rw` token; give dashboards, alerting, and
  anything read-only a `ro` token. A `ro` token cannot ingest, flush, or
  annotate — and over MCP it reaches the read tools but not the writer.
- Mint `admin` tokens for operators and compliance tooling only, and never
  hand one to a telemetry producer. Erasure is the one destructive verb in
  the API, and it is gated on this scope precisely so that a compromised or
  overprivileged ingest credential cannot destroy the telemetry it writes.
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

A tenant's cutoff is resolved in one order: **its override, else
`--ttl-seconds`, else never.** A tenant with no override on a server with no
global TTL keeps everything, whatever other tenants are configured with.
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
is the semantic verdict; its `conclusive` flag is false whenever an
over-approximate check — the byte-level occurrence scans over the log and
the rollup sidecars — found the subject's identifiers without proving them
benign, and whenever any domain could only reach `attention`. The
subcommand's exit code carries the distinction: `0` erased and conclusive,
`3` erased but inconclusive (read the attention domains), `2` not erased.

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

## See also

- [Deployment](deployment.md) — process topology, disk layout, service setup
- [Monitoring](monitoring.md) — what to watch and what to alert on
- [Configuration reference](../configuration.md) — every flag and its default
