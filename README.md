# Traza

**A trace datastore with first-class LLM and agent observability — one binary from laptop to cluster.**

Traza (Spanish for "trace") ingests OpenTelemetry or plain-JSON spans over HTTP, stores them durably, and answers trace lookups, filtered searches, and token/cost analytics in milliseconds — with a trace browser and no infrastructure to stand up. Two dependencies (`serde`, `serde_json`), no external database, and one deployment story at every size: today a single node that starts in milliseconds; the designed trajectory is replicated, highly available clusters of the same binary.

```sh
cargo build --release
(cd ui && npm ci && npm run build)          # builds the dashboard into ui/dist
./target/release/traza-server --data-dir ./data --port 8080
# open http://localhost:8080 — the server serves ui/dist
```

```sh
curl -X POST http://localhost:8080/v1/spans -H 'Content-Type: application/json' \
  -d '[{"trace_id":"trace-1","span_id":"span-1","name":"charge","service":"checkout",
        "start_time_unix_nano":1700000000000000000,
        "end_time_unix_nano":1700000000002500000,
        "status":"ok","attributes":{"region":"us-east"}}]'
# {"accepted":1,"durability":"wal"}

curl http://localhost:8080/v1/traces/trace-1
```

Full walkthrough: **[Getting started](docs/guide/getting-started.md)**.

## Documentation

The complete set lives in **[docs/](docs/README.md)**, organised by what you are
doing.

**Using Traza** — [getting started](docs/guide/getting-started.md) ·
[data model](docs/guide/data-model.md) · [ingest](docs/guide/ingest.md) ·
[HTTP API reference](docs/guide/http-api.md) ·
[LLM semantics](docs/llm-semantics.md) ·
[trace browser](docs/guide/trace-browser.md)

**Operating Traza** — [deployment](docs/operations/deployment.md) ·
[durability](docs/operations/durability.md) ·
[administration](docs/operations/administration.md) ·
[monitoring](docs/operations/monitoring.md) ·
[capacity](docs/operations/capacity.md) ·
[configuration reference](docs/configuration.md)

**Changing Traza** — [architecture](docs/internals/architecture.md) ·
[invariants](docs/internals/invariants.md) ·
[module map](docs/internals/module-map.md) ·
[testing](docs/internals/testing.md) ·
[benchmarking](docs/internals/benchmarking.md) ·
[segment format](docs/segment-format.md) · [CONTRIBUTING.md](CONTRIBUTING.md)

**Direction** — [roadmap](docs/roadmap.md) · [HA design](docs/ha-design.md)

## Why Traza

Most tracing backends make you assemble a fleet before the first span: a column store or search cluster, a queue in front, an operator keeping it healthy. Traza's bet is that a trace datastore should scale like a database, not like a pipeline — **one binary whose deployment grows with you** instead of a different architecture at every size:

- **Start on one machine in seconds.** A single process stores everything under `--data-dir` and needs nothing else — right-sized for a laptop, a CI job, a single-host service, an edge box, or the AI agent you're debugging *right now*.
- **Scale by adding nodes, not systems.** The engine's foundations — immutable segments, idempotent primary-key ingest, journaled compaction — were chosen to replicate. The [HA design](docs/ha-design.md) (quorum-replicated logical log, validated full-state snapshots for catch-up) is the committed trajectory. Today's scope is single-node; see [Status](#status-and-roadmap).
- **Built for LLM and agent workloads.** Sessions, token and cost analytics, prompt/completion capture with large-payload offloading, post-hoc evals and feedback, and one-command dataset export — first-class, not bolted on.
- **OpenTelemetry-compatible, OpenLLMetry-native.** Point any OTel SDK at it with two environment variables. Traza follows the [OpenLLMetry](https://github.com/traceloop/openllmetry) standard (`gen_ai.*` / `traceloop.*`), so instrumented apps get sessions and token/cost analytics with no attribute renaming.
- **Small enough to trust.** Two direct dependencies; HTTP, threading, and file I/O are the Rust standard library. `#![forbid(unsafe_code)]`. Every performance number is measured by a bundled benchmark, never estimated.
- **Crash-safe by construction.** Immutable segments written by write-temp, fsync, atomic rename; recovery loads only complete segments and heals crash artifacts.

## What it does

**Span identity is a primary key.** `(trace_id, span_id)` uniquely names a span; re-ingesting it replaces the stored version (last write wins). Client retries are idempotent and never create duplicates — no idempotency key, no client-side deduplication. See the [data model](docs/guide/data-model.md).

**Two ingest surfaces.** `POST /v1/spans` takes a plain JSON array; `POST /v1/traces` takes OTLP/HTTP as binary protobuf or JSON. Point any OTel SDK at Traza with `OTEL_EXPORTER_OTLP_ENDPOINT` and `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf`. gRPC is not served. See [ingest](docs/guide/ingest.md).

**LLM and agent telemetry lands queryable without translation.** `gen_ai.*`, `llm.usage.*`, and `traceloop.*` attributes drive sessions, provider/model rollups, and token/cost analytics directly:

```sh
curl 'http://localhost:8080/v1/sessions?limit=20'          # per session: spans, traces, tokens, cost, errors
curl 'http://localhost:8080/v1/sessions/chat-4711'         # + the per-trace breakdown
curl 'http://localhost:8080/v1/stats/llm?group_by=model'   # or provider | service | session | day
```

Prompt and completion payloads above `--payload-threshold-bytes` are offloaded to a content-addressed store and replaced inline by a `$payload` reference, so a repeated system prompt is stored once. Evals and human feedback attach after the fact via `POST /v1/annotations` without mutating spans, and `GET /v1/export` turns any search into a streaming NDJSON dataset. Conventions and query recipes: [LLM semantics](docs/llm-semantics.md).

**A trace browser, served from its build output.** The dashboard is a React app in [`ui/`](ui/) that `traza-server` serves straight from `ui/dist` — nothing is compiled into the binary, so building the server needs no Node toolchain and a rebuilt UI is picked up without a restart. A packaged binary without a build ships the **API only**: `/` then returns a 404 explaining how to build it, and startup logs every path searched. See [trace browser](docs/guide/trace-browser.md) and [deployment](docs/operations/deployment.md#serving-the-dashboard).

**Durability you choose and the server states.** `--durability` is `buffered`, `wal` (default), or `flushed`, and every ingest response echoes the mode so a client never has to guess what its `200` meant. One caveat stated plainly: `fsync` on **macOS does not flush the drive's own write cache**, so a power cut there can still lose an acknowledged write; a kill -9, a panic, or an OS crash cannot, on either platform. See [durability](docs/operations/durability.md).

**Span payloads are never resident; indexed attribute *values* are.** Segments are file-backed and payloads are read on demand, so a store larger than RAM serves correctly — as long as what you index is low-cardinality. Measured: 10M spans with six enum-valued attributes open in 846 MiB, and the cost is just 8 bytes per span per indexed attribute. But the attribute index is keyed on the whole value, so indexed prompt text stays in RAM verbatim: one 2 KiB prompt per span measures **RSS ≈ 1.44 × the prompt text ingested**, which is O(data), not O(indexes). Lower `--payload-threshold-bytes` to keep prompts out of the index. Filtered search costs one index probe per segment, which is what size-tiered compaction (on by default) exists to bound. Measured figures and their trade-offs: [capacity](docs/operations/capacity.md).

## HTTP API

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/v1/spans` | Ingest a JSON span batch |
| `POST` | `/v1/traces` | OTLP/HTTP ingest (protobuf or JSON) |
| `GET` | `/v1/spans?…` | Filtered span search |
| `GET` | `/v1/traces/{trace_id}` | One trace's spans plus its annotations |
| `GET` | `/v1/sessions`, `/v1/sessions/{id}` | Sessions and per-trace breakdown |
| `GET` | `/v1/stats/llm?group_by=…` | Token/cost aggregation |
| `POST` / `GET` | `/v1/annotations` | Attach and query scores and feedback |
| `GET` | `/v1/payloads/{reference}` | Raw bytes of an offloaded payload |
| `GET` | `/v1/export?…` | Streaming NDJSON export with completion trailers |
| `GET` | `/v1/stats`, `/v1/metrics` | Store statistics; Prometheus metrics |
| `POST` | `/v1/flush` | Force buffered spans into a durable segment |

Every parameter, response shape, and error is in the **[HTTP API reference](docs/guide/http-api.md)**.

## Using Traza as a library

The engine embeds in your own process — same durability, no server:

```rust
use traza::{Config, SpanFilter, Store};

let store = Store::open("./data", Config::default())?;
store.ingest(span)?;          // buffered; flushes automatically at the threshold
store.flush()?;               // or force a durable segment now
let slow = store.query(&SpanFilter {
    service: Some("checkout".into()),
    min_duration_ns: Some(5_000_000),
    ..SpanFilter::default()
})?;
```

A complete, runnable version is in [ingest](docs/guide/ingest.md#using-the-engine-directly). A data directory has exactly one writer, so an embedding process must not also run a server against it.

## Design

Two layers with one contract: never lose a completed write, never serve a torn one.

The **storage engine** buffers spans in memory, appends them to a write-ahead log, and flushes sorted, immutable segment files — JSON payloads with embedded record-offset, trace, and attribute indexes — via write-temp, fsync, atomic rename. Opening a store parses only the indexes; spans materialize on demand. Filters narrow candidates through the indexes, then re-verify every predicate against the parsed span: an index accelerates a filter, it never changes its semantics.

The **HTTP server** is a deliberately small HTTP/1.1 implementation on `std::net`, bounded by concurrent connections rather than a queue, in front of the engine — which is its only datastore. There is no server-side log or side index; restart durability is the engine's.

Deeper: [architecture](docs/internals/architecture.md) · [invariants](docs/internals/invariants.md) · [segment format](docs/segment-format.md).

## Performance

Measured on macOS/aarch64 (10 hardware threads) by the bundled benchmarks over corpora ingested through the real HTTP path:

- **Sustained ingest:** 116,618 spans/s single client, 208,973 spans/s at 16 concurrent clients (`wal`)
- **Trace lookup:** p95 0.64 ms · **Attribute-filtered search:** p95 3.3 ms (1M-span corpus)
- **Compaction is worth 16–28x on filtered search at 100M spans**, and the segment-size cap is worth another 3–4x on top — at a real cost in memory and ingest throughput

Full percentiles, the 10M and 100M scaling runs, the ingest matrix, and an honest list of what is **not** measured: **[capacity](docs/operations/capacity.md)**. The underlying records are [BENCHMARKS.md](BENCHMARKS.md) and [INGEST-BENCHMARK.md](INGEST-BENCHMARK.md), both rewritten by the benchmarks themselves — run them on your hardware rather than trusting ours.

## Status and roadmap

Traza is pre-1.0 and honest about it: on-disk formats may change between 0.x versions, and single-node is the current scope. Shipped and load-bearing today: durable segment storage with a write-ahead log and crash recovery, size-tiered compaction, OTLP protobuf/JSON ingest, sessions and cost analytics, payload offloading, annotations, streaming export, bearer auth, Prometheus metrics, and the [`ui/`](ui/) trace browser served from its build output.

Known architectural gap: query-visible state lives in several independent recovery domains (the write-ahead log and buffer, segments, annotations, payload files), and nothing yet names one state they all agree on. Backup, export, retention and replication are consequently four mechanisms rather than one. The generation/checkpoint boundary that fixes the class is designed in [docs/generations-design.md](docs/generations-design.md) and scheduled before 1.0.

The destination is bigger than one node. The full product roadmap — durable v1 foundations, then replicated HA clusters and agent-native debugging depth, then columnar analytics at billion-span scale, then the enterprise control plane — lives in [docs/roadmap.md](docs/roadmap.md), with the HA architecture detailed in [docs/ha-design.md](docs/ha-design.md). Same binary, same API, at every phase.

Deliberately out of scope: a metrics/logs suite, embedded eval models, general SQL, and framework SDKs — the [roadmap](docs/roadmap.md#explicit-non-goals) explains why.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). The short version: stable Rust is the only dependency, `./ci.sh` is the merge bar, and new dependencies need a reason.

## License

Licensed under the [Apache License, Version 2.0](LICENSE). Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be licensed as above, without any additional terms or conditions.
