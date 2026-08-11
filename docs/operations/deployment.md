# Deployment

## The shape of a Traza deployment

One process, one data directory, one port. There is no database to provision,
no queue to run in front, and no coordinator. `traza-server` links the storage
engine directly; the server *is* the datastore.

```sh
traza-server --data-dir /var/lib/traza --host 127.0.0.1 --port 8080
```

Startup is milliseconds, and the process announces exactly what it is
promising:

```
traza-server listening on 127.0.0.1:8080
traza-server: durability=wal — acknowledged writes are fsynced to the write-ahead log and recovered on restart
traza-server serving dashboard from /opt/traza/ui
```

Every flag is documented in the [configuration reference](../configuration.md).
This page covers the deployment decisions around them.

## One writer per data directory

**A data directory has exactly one writer.** `Store::open` takes an exclusive
lock on it and a second opener is rejected. This applies to every writer,
not just servers: the `seed --data-dir` tool and any process embedding the
engine as a library count too.

The lock records its owner's PID. If the owner dies without cleaning up —
SIGKILL, OOM, power loss — the lock is **stale** and the next open reclaims it
automatically. A lock naming a live process still rejects the open, and an
unreadable lock file is treated as live: a false negative merely keeps the
conservative rejection, which never corrupts data.

Practical consequences:

- Do not run two servers against one directory, and do not put a data directory
  on shared storage that two hosts can mount at once. The PID check is
  meaningless across hosts.
- To bulk-load with `seed --data-dir`, stop the server first; or leave it
  running and use `seed --url` to go through the API instead.
- Scaling today is vertical; multi-node replication is not shipped behaviour.

## What lives on disk

Everything is under `--data-dir` (default `./data`), created if missing.

```
data/
  LOCK                                  single-writer lock, holds the owner PID
  wal.log                               write-ahead log; truncated at each flush
  segment-00000000000000000000.seg      immutable sorted segments
  segment-00000000000000000001.seg
  annotations.jsonl                     append-only annotation log
  payloads/
    29/29927e…fa.bin                    content-addressed offloaded payloads
    66/660f56…51.bin
  .supersede.<first-output>.journal     transient: an in-flight compaction merge
  .segment-….tmp                        transient: an in-flight segment write
```

Notes that matter operationally:

- **Segments are immutable and named by a zero-padded id.** Path order is
  recency order, and correctness depends on it — never rename, reorder, or
  hand-edit a segment file.
- **`wal.log` is truncated, not rotated.** Its size is the work a restart would
  replay, and it drops to zero after each flush. `wal_bytes` in
  [`GET /v1/stats`](../guide/http-api.md#get-v1stats) reports it.
- **Dotfiles are transient.** A `.tmp` is an interrupted segment write; a
  `.supersede.*.journal` is an interrupted compaction. Both are cleaned up
  automatically at the next open. Do not delete them by hand while a server is
  running, and do not delete a `.journal` from a stopped server — it is what
  recovery uses to finish the rewrite correctly.
- **Disk sizing** should assume more than the logical span volume: superseded
  versions of re-ingested spans stay on disk until compaction rewrites their
  segment, and a merge needs room for its output before its inputs are removed.

## Networking

Traza speaks plain HTTP/1.1. **TLS is reverse-proxy territory** — put nginx,
Caddy, or your load balancer in front and terminate there.

The default bind is `127.0.0.1`. Binding anywhere else without authentication
is **refused at startup**:

```
traza-server: refusing unauthenticated non-loopback bind 0.0.0.0; configure TRAZA_TOKENS or pass --allow-unauthenticated-non-loopback explicitly
```

Configure `TRAZA_TOKENS`, or pass the explicit escape hatch if the network is
genuinely trusted. See
[administration](administration.md#authentication).

`--port 0` binds an ephemeral port and announces the real one on stderr, which
is how the test suite discovers it.

### Concurrency

`--max-connections` (default 1024) bounds **concurrent connections**, not
queued requests. Keep-alive means a persistent connection occupies its handler
until the client is done with it, so queueing past the limit would leave
clients waiting indefinitely instead of being told the server is full. Past the
limit a client gets an immediate `503`.

Size it against the number of client connections you expect, not the request
rate: a hundred exporters holding keep-alive connections need a hundred slots
regardless of how often they send.

> `--max-connections` replaced the older `--workers` flag. If you have a
> deployment script passing `--workers`, the server will now refuse to start
> with `unknown argument: --workers`.

Two more bounds worth knowing: request bodies are capped at 64 MiB and headers
at 64 KiB, and a connection is recycled after 100,000 requests.

### File descriptors

**Every segment holds an open file descriptor.** With compaction enabled the
segment count stays bounded, but a large store with `--compaction-fanout 0`
accumulates segments without limit — a measured ~10,100 segments at 100M spans,
which would exhaust a default 1024-fd limit. Raise `ulimit -n`, or better,
leave compaction on.

## Serving the dashboard

The dashboard is a **build artifact, not part of the binary**. `traza-server`
compiles with no Node toolchain and embeds no HTML; it serves whatever built
dashboard it finds on disk. A `cargo install`ed or otherwise packaged binary
therefore ships the **API only** until you give it a build.

With no `--ui-dir`, the server searches in this order and takes the first
directory containing an `index.html`:

1. `$TRAZA_UI_DIR` — for operators who put it anywhere else
2. `<directory of the executable>/ui` — the packaging convention
3. `<directory of the executable>/../share/traza/ui` — the Unix prefix convention
4. `./ui/dist` — a working copy after `npm run build`

Packagers should drop the build beside the executable as `ui/`. From a
checkout, `cd ui && npm ci && npm run build` puts it at `./ui/dist`.

If none is found, **nothing breaks**: the API runs exactly the same, `/`
returns a `404` explaining how to build it, and startup logs every path it
searched. That last part matters — "no dashboard at ./ui/dist" tells an
operator running an installed binary from some other directory nothing at all.

Because it is read from disk rather than compiled in, a rebuilt UI is picked up
**without restarting the server**.

The dashboard shell is served *before* the authentication gate — it is static
build output carrying no data — while every `/v1` call it makes stays gated.
Path traversal outside the build root is refused against the canonicalized
root.

## Running as a service

Traza needs no special privileges. Run it as an unprivileged user that owns the
data directory, behind a TLS-terminating proxy, with the token file supplied
through the environment rather than the command line (arguments are visible in
`ps`).

A minimal systemd unit:

```ini
[Unit]
Description=Traza trace datastore
After=network.target

[Service]
User=traza
# A file with mode 0600 containing TRAZA_TOKENS=rw:…,ro:…
EnvironmentFile=/etc/traza/tokens.env
ExecStart=/opt/traza/bin/traza-server --data-dir /var/lib/traza --host 127.0.0.1 --port 8080
Restart=on-failure
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

**Shutdown.** There is no graceful-shutdown handshake: the process is safe to
stop at any moment, which is the whole point of the durability contract. What a
restart preserves depends on the mode — in `wal` and `flushed`, everything
acknowledged; in `buffered`, nothing not yet sealed. Read
[durability](durability.md) before choosing. If you want to minimise replay
work at the next start, `POST /v1/flush` before stopping.

**Upgrades** are a binary swap and a restart. On-disk formats may change
between 0.x versions, so read the [changelog](../../CHANGELOG.md) before
upgrading a store you care about — pre-1.0, a format change may mean existing
segments are no longer read.

## Health checks

`GET /v1/stats` is the natural liveness and readiness probe: it is cheap
(O(number of segments), never a corpus decode) and it fails if the store cannot
be read. When authentication is on it needs a `ro` token like any other route.

For alerting, use [`GET /v1/metrics`](monitoring.md).

## Next

- [Durability](durability.md) — what an acknowledged write means
- [Administration](administration.md) — auth, retention, compaction, backups
- [Monitoring](monitoring.md) — metrics and what to alert on
- [Capacity](capacity.md) — measured performance characteristics
- [Configuration reference](../configuration.md) — every flag
