# Getting started

From nothing to a span you can see in the browser. Nothing else needs to be
installed: no database, no queue, no mandatory container.

## 1. Install

The fastest path is a [release archive](https://github.com/toshish/traza/releases)
— the server with the dashboard already built, no toolchains:

```sh
curl -LO https://github.com/toshish/traza/releases/download/v0.24.2/traza-0.24.2-macos-aarch64.tar.gz
tar xzf traza-0.24.2-macos-aarch64.tar.gz && cd traza-0.24.2-macos-aarch64
```

(`linux-x86_64` and `linux-aarch64` likewise; the container image is
`ghcr.io/toshish/traza`, and `cargo install traza --locked --bin traza-server`
installs the server API from crates.io — the dashboard comes from an archive
or a `ui/` build.)

Or build from source: stable Rust (1.81 or newer) builds the server, Node 22
or newer builds the dashboard — only if you want the dashboard; the API does
not need it.

```sh
git clone https://github.com/toshish/traza.git
cd traza
cargo build --release
(cd ui && npm ci && npm run build)   # optional: emits ui/dist
```

`cargo build --release` produces `traza-server` and the tooling binaries in
`target/release/`:

| Binary | Purpose |
|---|---|
| `traza-server` | The HTTP server |
| `seed` | Loads a realistic demo corpus, for trying the UI |
| `bench`, `ingest-bench`, `query-bench`, `storage-bench`, `index-mem-bench`, `content-bench` | Benchmarks (see [benchmarking](../internals/benchmarking.md)) |

## 2. Run the server

```sh
./target/release/traza-server --data-dir ./data --port 8080
```

It starts in milliseconds and tells you exactly what it is promising:

```
traza-server listening on 127.0.0.1:8080
traza-server: durability=wal — acknowledged writes are fsynced to the write-ahead log and recovered on restart
traza-server serving dashboard from /path/to/traza/ui/dist
```

If you skipped the UI build, the third line is replaced by a list of every
directory searched. That is not an error — the API is unaffected, and `GET /`
returns a 404 explaining how to build the dashboard.

The default bind is loopback. Binding anywhere else without authentication is
refused; see [administration](../operations/administration.md#authentication).

## 3. Send a span

A span batch is a JSON array, or an object with a `spans` key. Both halves of
the primary key (`trace_id`, `span_id`) must be non-empty; everything else has
a sensible default.

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
        "status": "ok",
        "attributes": {"region": "us-east", "http.method": "POST"}
      }]'
```

```json
{"accepted":1,"durability":"wal"}
```

The response names what the acknowledgement guarantees, so a client never has
to infer it. See [durability](../operations/durability.md).

## 4. Read it back

Fetch the whole trace:

```sh
curl http://localhost:8080/v1/traces/trace-1
```

```json
{"annotations":[],"spans":[{"attributes":{"http.method":"POST","region":"us-east"},"end_time_ns":1700000000002500000,"events":[],"name":"charge","parent_span_id":null,"service":"checkout","span_id":"span-1","start_time_ns":1700000000000000000,"status":"ok","trace_id":"trace-1"}],"trace_id":"trace-1"}
```

Search across traces. Filters are ANDed; `attr.KEY` matches one attribute
exactly and may be repeated:

```sh
curl 'http://localhost:8080/v1/spans?service=checkout&attr.region=us-east&min_duration_ms=2&limit=50'
```

Check what the store holds:

```sh
curl http://localhost:8080/v1/stats
```

```json
{"buffered_records":1,"bytes_on_disk":0,"durability":"wal","persisted_records":0,"record_count":1,"segment_count":0,"total_records":1,"wal_bytes":261}
```

`bytes_on_disk` and `segment_count` are zero because nothing has been sealed
into a segment yet — the span is durable in the write-ahead log
(`wal_bytes` is non-zero), not yet in a segment. `POST /v1/flush` seals it
immediately if you want to watch that happen.

The full route list is in the [HTTP API reference](http-api.md).

## 5. See it in the browser

Open <http://localhost:8080/>. If you built `ui/dist` in step 1, the trace
browser loads: recent spans with a filter bar, per-trace waterfalls, span
detail, sessions, and LLM analytics. See the
[trace browser guide](trace-browser.md).

To get a feel for it with real-shaped data rather than one span, load the demo
corpus over the API:

```sh
./target/release/seed --url http://localhost:8080 --scale 3
```

```
seed: 580 spans (3/3 scale units)
seed: posted 580 spans and 9 annotations to http://localhost:8080
```

The corpus is deliberately messy in the ways production is messy: three
attribute dialects, tool-calling agent trees, multi-turn sessions, multimodal
and oversized payloads, failures with linked retries, a runaway agent that
cannot make progress, a large healthy fan-out that must not be mistaken for
one, and ordinary non-LLM traffic. Raise `--scale` for more. `seed --data-dir DIR` writes through the
engine directly instead, which is faster but requires that no server is running
against that directory — a data directory has exactly one writer.

If the server has authentication configured, give `seed --url` a token through
**`TRAZA_TOKEN`** — singular, and not to be confused with the server's own
`TRAZA_TOKENS`. It needs `rw`:

```sh
TRAZA_TOKEN=your-rw-token ./target/release/seed --url http://localhost:8080 --scale 3
```

Without it the request is rejected before its body is read, and the connection
closes — `seed` reports `Connection reset by peer`.

## 6. Point a real application at it

Any OpenTelemetry SDK exports to Traza with two environment variables:

```sh
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:8080
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf   # or http/json
```

Traza serves OTLP over HTTP, not gRPC. Apps instrumented with
[OpenLLMetry](https://github.com/traceloop/openllmetry) or the OpenTelemetry
GenAI conventions land queryable with no attribute renaming: sessions, token
counts, and model rollups populate on their own. See
[ingest](ingest.md) and [LLM semantics](../llm-semantics.md).

## 7. Let your agent read it

Traza can serve the [Model Context Protocol](mcp.md), so the coding agent in
your terminal can answer "why did last night's run cost four dollars" against
this store instead of you pasting JSON between two windows. Restart with
`--mcp`, then point a client at it:

```sh
traza-server --data-dir ./data --port 8080 --mcp
```

```sh
claude mcp add --transport http traza http://localhost:8080/v1/mcp
```

A scripted version of the whole thing — seed, serve, investigate, clean up —
is [`examples/mcp-demo/run.sh`](../../examples/mcp-demo/README.md).

Ask it to call `describe_store` first — service and model names differ per
store, and that call is what stops an agent guessing one and reporting that
nothing is wrong. The dashboard's **MCP** screen shows the live tool list and
generates the configuration for clients that need a stdio subprocess instead.

## Where to go next

- [Data model](data-model.md) — what a span is, and why re-ingesting one is safe
- [HTTP API reference](http-api.md) — every route and parameter
- [MCP server](mcp.md) — the tools, resources and prompts an agent gets
- [Deployment](../operations/deployment.md) — running it somewhere that matters
- [Configuration reference](../configuration.md) — every flag
