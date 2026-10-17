Traza
=====

**Traza is the flight recorder for AI agents.** It records every model call and tool call your agents' instrumentation exports, stops a run that is going wrong before its next step, replays a recorded run with zero model calls, and proves afterwards what it stored.

Underneath, Traza is a trace database for LLM and agent workloads. It runs as a single binary with no external database, no queue, and no coordinator: OpenTelemetry in, one directory on disk, and an HTTP API, a dashboard and an MCP endpoint out.

**Trace lookup at p95 1.3 ms and filtered search at p95 5.4 ms over a million spans. Stores 0.16–0.23x the bytes you send on service-trace and LLM-call shapes.** One process, one directory.

[![CI](https://github.com/toshish/traza/actions/workflows/ci.yml/badge.svg)](https://github.com/toshish/traza/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/traza)](https://crates.io/crates/traza)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)

## Record

Point any OpenTelemetry exporter at it. Agents instrumented with OpenLLMetry, OpenInference or the OpenTelemetry GenAI conventions arrive with sessions, token and cost analytics, and step-by-step runs already populated. No attribute renaming, no mapping file.

```sh
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:8080
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
export OTEL_EXPORTER_OTLP_COMPRESSION=gzip
python weather_agent.py                       # your instrumented agent

curl -s localhost:8080/v1/tapes/weather-7     # its model and tool calls in order, with what went in and came out
```

A run is a session (`session.id`, `gen_ai.conversation.id`, or OpenLLMetry's association property) or, without one, a trace. [Ingest](docs/guide/ingest.md) · [steps and runs](docs/llm-semantics.md#steps-and-runs)

## Guard

A run fuse holds a run to limits on cost, tokens, steps, repeated calls and failure streaks:

```sh
curl -X POST http://127.0.0.1:8080/v1/fuses/policies -d '{
  "name": "support", "match": {"service": "support-agent"},
  "limits": {"max_cost_usd": 2.0, "max_steps": 40, "max_repeats": 4, "max_error_streak": 3},
  "action": "stop"}'
```

When a `support-agent` run makes the same `search_orders` call a fourth time, the acknowledgement of that batch carries the verdict:

```
HTTP/1.1 200 OK
Traza-Fuse: stop; run=sess-7f3a
```

A span arrives when its step has ended, so that verdict is about the *next* step. An agent that asks before each step (`POST /v1/fuses/check`, from its harness, the [client libraries](docs/guide/clients.md#the-guard) or the [Claude Code hook](examples/guard/README.md#the-claude-code-hook)) hears the stop before the step runs instead. A kill switch stops any run at once, and a reset never refills a spent budget. A fuse acts between steps on recorded usage; it never interrupts a call in flight. [Run fuses](docs/guide/guard.md) · [limitations](docs/guide/guard.md#limitations)

## Replay

Re-run a recorded run through the agent's own code, with every model answer served from the recording:

```sh
curl -s -X POST localhost:8080/v1/replays -H "Authorization: Bearer $TRAZA_TOKEN" \
  -d '{"run": "weather-7"}'                   # answers its id and base_urls for both SDKs

export OPENAI_BASE_URL=http://localhost:8080/v1/replays/rp_ac879985abef9cba12bb342bdc3b8bd1/openai/v1
export OPENAI_API_KEY=$TRAZA_TOKEN
python weather_agent.py                       # zero model calls; ANTHROPIC_BASE_URL works the same way
```

Strict mode refuses a request that is not the recorded one (`409 replay_divergence`, naming the step) rather than answer a different question, and `GET /v1/diff?a=weather-7&b=weather-8` names the first step where two runs parted, and why. Tools still run live unless your harness takes their recorded results from `tools/next`, and streams are rebuilt from the stored text. On a server with no credentials, any non-empty API key works. [Replay](docs/guide/replay.md) · [limits](docs/guide/replay.md#limits-stated-plainly)

## Prove

Every published generation, every settled erasure (as a SHA-256 of its subject, never the trace, span or session id) and every external anchor adds one entry to a SHA-256 hash chain:

```sh
curl -s localhost:8080/v1/ledger/verify
# {"chain_verified_at_ns":…,"entries":5,"generation":{…},"head":"34e6…","intact":true,"problems":[],…}

curl -s localhost:8080/v1/evidence -o evidence.json   # one document, one digest for a notary to sign
./traza-server verify --ledger --data-dir ./data      # offline, on a directory no server owns: exit 0 intact, 2 broken
```

A ten-line standard-library Python script recomputes every head. The chain has no secret in it, so history is tamper-evident only against an anchor, a head countersigned by a notary, that you keep outside the store; Traza stores anchors and never checks their signatures. [Ledger](docs/guide/ledger.md) · [what it proves, and what it does not](docs/guide/ledger.md#what-it-proves-and-what-it-does-not)

## See it run

![Three of the nine demos, replayed: a million spans ingested in 6.1 seconds and answered in microseconds to milliseconds; a kill -9 with all 33,000 acknowledged spans recovered; an agent diagnosing a runaway session over MCP and proving the fix with an experiment diff.](docs/assets/demo-reel.svg)

*Replayed from real runs of the [demo tour](examples/README.md) — nine scripted proofs, one
command each. Every number a demo prints is measured on your machine while you watch, each
script asserts its own claims, and CI runs all nine. Clone, then pick one:*

```sh
./examples/swarm/run.sh      # a live agent cockpit in the dashboard, until Ctrl-C
./examples/crash/run.sh      # kill -9 mid-ingest; every acknowledged span survives
./examples/needle/run.sh     # a million spans in, one sentence found in milliseconds
./examples/incident/run.sh   # an agent diagnoses a runaway over MCP, promotes, diffs
./examples/vanish/run.sh     # tenant-precise erasure, and a receipt that names the backup
./examples/mcp-demo/run.sh   # the whole MCP tool surface, including everything it refuses
./examples/guard/run.sh      # a looping agent stopped between steps; a Claude Code session too
./examples/replay/run.sh     # record an agent run, replay it with zero model calls
./examples/prove/run.sh      # a hash chain recomputed with hashlib; one flipped byte, exit 2
```

## Install

### Download

The server with the dashboard already built in. No Rust, no Node.

```sh
VERSION=0.27.0
PLATFORM=macos-aarch64          # or linux-x86_64, linux-aarch64

curl -LO https://github.com/toshish/traza/releases/download/v$VERSION/traza-$VERSION-$PLATFORM.tar.gz
tar xzf traza-$VERSION-$PLATFORM.tar.gz
cd traza-$VERSION-$PLATFORM
```

### Docker

```sh
docker run -p 8080:8080 -v traza-data:/data \
  -e TRAZA_TOKENS="rw:$(openssl rand -hex 16)" \
  ghcr.io/toshish/traza:latest
```

One container per volume at a time: a restarted container reclaims its own lock, so nothing stops a second one started on the same volume. Stop the old container before starting its replacement ([why](docs/operations/deployment.md#one-writer-per-data-directory)).

### Cargo

```sh
cargo install traza --locked --bin traza-server
```

Installs the server and API. The dashboard ships with the release archives, or build it from [`ui/`](ui/).

To embed the engine directly in your own process instead:

```sh
cargo add traza
```

### From source

```sh
git clone https://github.com/toshish/traza && cd traza
cargo build --release
(cd ui && npm ci && npm run build)
```

## Run

```sh
./traza-server
```

```
traza-server listening on 127.0.0.1:8080
traza-server: durability=wal — acknowledged writes are fsynced to the write-ahead log and recovered on restart
traza-server: profile=balanced — flush-spans=10000, wal-commit-window=off
traza-server: run fuses on — global policy: warn at {"max_repeats":12}
traza-server serving dashboard from ./ui/dist
```

That is the whole setup. Data lands in `./data`, the dashboard is on <http://localhost:8080>.

### Common options

| Flag | Default | |
|---|---|---|
| `--data-dir DIR` | `./data` | All state. One writer process per directory. |
| `--host ADDR` | `127.0.0.1` | A non-loopback bind requires `TRAZA_TOKENS` or a runtime credential. |
| `--port PORT` | `8080` | `0` binds an ephemeral port and announces it. |
| `--durability MODE` | `wal` | `buffered`, `wal`, or `flushed`. Every response says which one answered. |
| `--profile NAME` | `balanced` | `throughput`, `balanced`, or `latency`. Sets the write-path knobs together. |
| `--ttl-seconds N` | off | Retention window for spans, annotations and payloads. |
| `--mcp` | off | Serve Model Context Protocol at `/v1/mcp`. |
| `--fuse-max-cost-usd F`, `--fuse-max-steps N`, … | warn at 12 repeats | The global [run fuse](docs/guide/guard.md) policy, for runs no tenant policy covers. `--fuse-action stop` makes it stop; `--no-fuses` turns fuses off. |
| `--ui-dir DIR` | beside the binary | Where the built dashboard lives. |
| `--restore DIR` | | Install a backup into `--data-dir`, then serve it. |
| `TRAZA_TOKENS` | unset | Bearer auth: `rw:` and `ro:` scoped, plus `admin:` for erasure, backups and `/v1/admin/*`. Bind a credential to one tenant with `rw@acme:token`. Credentials can also be minted and revoked at runtime. |

`--help` prints all forty-six. The [configuration reference](docs/configuration.md) explains what each one costs.

### Examples

**Serving a team.** Named paths, an open bind address with auth, thirty days of retention, and the agent endpoint on:

```sh
export TRAZA_TOKENS="rw:$(openssl rand -hex 16),ro:$(openssl rand -hex 16)"

./traza-server \
  --data-dir /var/lib/traza \
  --host 0.0.0.0 \
  --ttl-seconds 2592000 \
  --mcp
```

**Closing an open store without a restart.** On a loopback start with no credentials, mint the first one. It must be an unbound `admin`, its token is in this response and nowhere else, and from this request on every call must authenticate, across restarts too. The rest are minted, listed and revoked under `/v1/admin/credentials` with that token:

```sh
curl -X POST http://localhost:8080/v1/admin/credentials \
  -H 'Content-Type: application/json' -d '{"scope":"admin","label":"root"}'
```

**Bulk backfill.** The `throughput` profile seals larger segments and lets more acknowledgements share one fsync, which is what you want when nothing is waiting on any single batch:

```sh
./traza-server --data-dir /var/lib/traza --profile throughput
```

**A client blocking on the acknowledgement.** The `latency` profile trades peak ingest for a materially better p95:

```sh
./traza-server --data-dir /var/lib/traza --profile latency
```

**Tests and CI.** `buffered` is the fastest mode and lossy by design, which is exactly right for a store you are about to throw away. Port `0` picks a free port and prints it, so parallel test runs do not collide:

```sh
./traza-server --data-dir "$(mktemp -d)" --port 0 --durability buffered
```

**Debugging an agent from your terminal.** Serve MCP, then point a client at it:

```sh
./traza-server --mcp
claude mcp add --transport http traza http://localhost:8080/v1/mcp
```

**Backing up a running server.** Pin and verify a consistent copy, take it, then release the pin. With auth on, a pin takes an unbound `admin` token, as erasure takes an `admin` one:

```sh
curl -X POST http://localhost:8080/v1/backups/nightly
cp -a ./data/pins/nightly /backups/traza-$(date +%F)
curl -X POST http://localhost:8080/v1/backups/nightly/release
```

**Restoring one.** Verified before anything is swapped, then served:

```sh
./traza-server --data-dir /var/lib/traza --restore /backups/traza-2026-08-10
```

**Erasing a session, and proving it.** Deletion by trace, span, session, tenant or payload, published at a checkpoint; the receipt re-checks every domain by name:

```sh
curl -X POST http://localhost:8080/v1/erasures \
  -H 'Content-Type: application/json' \
  -d '{"subject": {"kind": "session", "session_id": "sess-42"}}'

curl http://localhost:8080/v1/erasures/1/verify
```

## Send a span

```sh
curl -X POST http://localhost:8080/v1/spans \
  -H 'Content-Type: application/json' \
  -d '[{
    "trace_id": "trace-1",
    "span_id": "span-1",
    "name": "charge",
    "service": "checkout",
    "start_time_unix_nano": 1700000000000000000,
    "end_time_unix_nano": 1700000000002500000,
    "status": "ok"
  }]'
```

```json
{"accepted":1,"durability":"wal"}
```

Or point an existing app at it with the OTLP exporter variables under [Record](#record). Both routes take gzip bodies, and a batch over an [admission cap](docs/configuration.md#server-flags) is refused whole with a `400` that names the cap and the span.

## What you get

![An agent swarm in the trace browser: a 14-span waterfall with the critical path marked, per-span model and token counts, and the run's duration, tokens and cost across the top.](docs/assets/trace-waterfall.png)

**Fast reads.** Trace lookup at p95 1.28 ms and filtered search at p95 5.4 ms over a million spans. Full-text search across prompt text returns a selective term in 0.57 ms where scanning takes 485 ms.

**One process.** No metadata database, no column store, no lock service, no object store to configure. Run the binary against a data directory; no control plane is required.

**A small surface.** Three direct dependencies and thirteen packages in a default build (the rest of the lockfile belongs to the opt-in `object-storage` feature), and a 5 MB binary. HTTP, threading and file I/O are the standard library, gzip is the crate's own inflate, and the crate is `#![forbid(unsafe_code)]`.

**Agent telemetry as the workload.** Sessions, token and cost rollups, prompts and completions with large ones offloaded and deduplicated, evals and human feedback attached after the fact, live tail, and one-command dataset export.

**An endpoint your agent can query.** `--mcp` serves Model Context Protocol from the same binary and port: sixteen tools shaped like the questions people actually ask, fourteen of them reads (a run's guard verdict, its tape, a run diff and recall among them) and two writers that stay off until enabled, with stored span text confined as untrusted and results bounded for a context window.

**Durability you choose.** Three acknowledgement modes, and every response states which one answered it. The suite proves them by killing the process, not by asserting.

**Backup without stopping.** One call pins and verifies a consistent copy of spans, annotations and payload bytes together. Restore is one flag.

**Archive to object storage (preview).** Behind the default-off `object-storage` feature, `traza-object` publishes a verified pin to S3-compatible storage as an immutable snapshot — then queries it in place by verified range reads, restores it whole, or deletes it. Archives are explicit historical copies, not tiering: the live writer is untouched, and local retention never silently reaches a published snapshot. See [docs/operations/object-storage.md](docs/operations/object-storage.md).

**Deletion with a receipt.** Erase a trace, a session, a whole tenant, or one offloaded payload from every domain — buffer, log, segments, annotations, payload files, datasets — then prove it: `verify --erasure` re-checks each domain by name and reports the result of each, down to the pinned backup that still holds the bytes and the dataset example that carries a promoted copy.

**Tenants in the key, not bolted on.** Span identity is `(tenant, trace_id, span_id)`, so two customers sharing a trace id can never overwrite each other. A token bound with `rw@acme:token` writes and reads exactly one tenant on every surface; retention takes per-tenant windows; `GET /v1/tenants` accounts usage per tenant. Single-tenant stores write byte-identical files and notice nothing.

**Operable while it runs.** Credentials are minted, listed and revoked over HTTP and kept only as digests. Each tenant can carry monthly span and byte quotas, an ingest pause and its own retention, set without a restart; a batch over quota is refused with `429` and `Retry-After`, and `GET /v1/usage` reports what each tenant has sent. `/healthz` and `/readyz` answer an orchestrator without a credential. See [administration](docs/operations/administration.md).

**The eval loop is representable.** Promote failing production traces into an immutable, content-addressed dataset version — examples keep their own copies, so deleting the source trace cannot corrupt them — run the experiment with your own harness, record runs and scores against `(experiment, example, span)`, and read score distributions and experiment-over-experiment diffs back out. Identity and addressing only: no runner, no scorer library, and your workflow stays yours.

## Client libraries

Zero-dependency clients for the routes an agent harness calls (the pre-call guard and the rest of the run fuses, replays, diffs, recall, the ledger head and evidence packs) live in [`sdk/`](sdk/): `traza` for Python 3.9+, on the standard library alone, and `@trazadb/sdk` for TypeScript on Node 20+, Deno, Bun and edge runtimes. They install from this repository. They are not instrumentation: OpenTelemetry records the spans, and the clients ask the server about them. See [docs/guide/clients.md](docs/guide/clients.md).

## Performance

| | |
|---|---|
| Trace lookup, 1M spans | p95 **1.28 ms** |
| Filtered search, 1M spans | p95 **5.4 ms** |
| Content search, selective term | **0.57 ms** (485 ms scanning) |
| Sustained ingest, `wal` | **208,973 spans/s** *(v6-format run, 2026-07)* |
| Stored vs ingested, service traces | **0.23x** |
| Stored vs ingested, LLM calls | **0.16x** |
| Direct dependencies | **3** |

The performance and storage figures come from bundled benchmarks: HTTP ingest/query harnesses and the embedded-store content-search harness. Latency and storage harnesses assert the format's [acceptance gates](docs/segment-format.md#acceptance-gates). The sustained-ingest row is explicitly historical; current v8 canonical runs sustained 65,749–69,615 spans/s with concurrent compaction. Both ingest figures also predate the per-span admission pass added in 0.27.0, which serializes every span once more to enforce the admission caps and charge usage, and have not been re-run since; [capacity](docs/operations/capacity.md) says what is and is not known about its cost. The paired [v7 → v8 comparison](docs/benchmarks/storage-v8-comparison.md) measured 31–44% fewer bytes across three synthetic corpora and identical normalized exports across migration. These are workload-specific measurements, not customer capacity guarantees. Run `cargo run --release --bin bench` to reproduce the canonical benchmark.

## When not to use Traza

**You require native HA or shared object storage.** Traza currently uses a single writer and local storage; this release does not add replication, failover, or an object tier. Format v8 measured 0.23x the ingested bytes on service traces and 0.16x on LLM calls in the bundled synthetic corpora. Repeated large context can also benefit from content addressing: 3.1 GiB ingested became 2.5 MiB on the highly repetitive `pinned-context` corpus. These results do not establish comparative HA cost or compression against other databases. [Storage economics and limitations](docs/storage-comparison.md) explains the distinction.

**You need metrics and logs in the same system.** Traza stores traces and their analytics. That is the whole surface, on purpose.

**You need horizontal scale-out today.** Traza is single-node.

## Documentation

Everything is in **[docs/](docs/README.md)** — getting started, the HTTP API, LLM semantics, the MCP server, run fuses, replay, the ledger, the client libraries, deployment, administration, durability, backup and restore, capacity, and the engine internals. What changed in each release is in the [changelog](CHANGELOG.md).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Stable Rust is the only dependency, `./ci.sh` is the merge bar, and a new dependency needs a written reason.

## License

Copyright © 2026 Toshish Jawale. [Apache-2.0](LICENSE).
