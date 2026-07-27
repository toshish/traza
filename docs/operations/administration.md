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

A comma-separated list of `scope:token` entries.

| Scope | Permits |
|---|---|
| `ro` | `GET` only |
| `rw` | `GET` and `POST` |

Tokens must be non-empty, unique, and free of whitespace and commas. Entries
may not have surrounding whitespace.

**An invalid `TRAZA_TOKENS` refuses startup.** Not a warning, not a fallback to
open — the process exits. Silently running open when the operator tried to
configure authentication would be the worst possible failure mode. The error
names the defect (`TRAZA_TOKENS contains an invalid scope`) and never echoes
the offending value.

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
the single writing tool additionally requires `rw` **and** `--mcp-annotations`.
A tool the presented token cannot call is not advertised to it.

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

## Backups

See [durability § Backups](durability.md#backups) — backup correctness is a
consequence of the crash-recovery model, so the two are documented together.

## See also

- [Deployment](deployment.md) — process topology, disk layout, service setup
- [Monitoring](monitoring.md) — what to watch and what to alert on
- [Configuration reference](../configuration.md) — every flag and its default
