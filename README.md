# Traza

**A trace datastore with first-class LLM and agent observability — one binary from laptop to cluster.**

Traza (Spanish for "trace") ingests OpenTelemetry or plain-JSON spans over HTTP, stores them durably, and answers trace lookups, filtered searches, and token/cost analytics in milliseconds — with a built-in trace browser and no infrastructure to stand up. Two dependencies (`serde`, `serde_json`), no external database, and one deployment story at every size: today a single node that starts in milliseconds; the designed trajectory is replicated, highly available clusters of the same binary.

```sh
cargo build --release
./target/release/traza-server --data-dir ./data --port 8080
# open http://localhost:8080 — the dashboard is built in
```

## Why Traza

Most tracing backends make you assemble a fleet before the first span: a column store or search cluster, a queue in front, an operator keeping it healthy. Traza's bet is that a trace datastore should scale like a database, not like a pipeline — **one binary whose deployment grows with you** instead of a different architecture at every size:

- **Start on one machine in seconds.** A single process stores everything under `--data-dir`, serves its own dashboard, and needs nothing else — right-sized for a laptop, a CI job, a single-host service, an edge box, or the AI agent you're debugging *right now*.
- **Scale by adding nodes, not systems.** The engine's foundations — immutable segments, idempotent primary-key ingest, journaled compaction — were chosen to replicate. The [HA design](docs/ha-design.md) (quorum-replicated logical log, segment shipping for catch-up) is the committed trajectory: clustered, highly available deployments running this same binary. Today's scope is single-node; see [Status](#status-and-roadmap).
- **Built for LLM and agent workloads.** Sessions, token and cost analytics, prompt/completion capture with large-payload offloading, post-hoc evals and feedback, and one-command dataset export — first-class, not bolted on.
- **OpenTelemetry-compatible.** Point any OTel SDK at it with two environment variables.
- **Small enough to trust.** Two direct dependencies; HTTP, threading, and file I/O are the Rust standard library. `#![forbid(unsafe_code)]`. Every performance number in [BENCHMARKS.md](BENCHMARKS.md) is measured by a bundled benchmark, never estimated.
- **Crash-safe by construction.** Immutable segments written by write-temp, fsync, atomic rename; recovery loads only complete segments and heals crash artifacts.

## Quickstart

Ingest a batch of spans (a JSON array, or `{"spans": [...]}`):

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
# {"accepted":1}
```

Fetch a trace, search spans, check the store:

```sh
curl http://localhost:8080/v1/traces/trace-1
curl 'http://localhost:8080/v1/spans?service=checkout&attr.region=us-east&min_duration_ms=2&limit=50'
curl http://localhost:8080/v1/stats
```

Or skip curl entirely: the dashboard at `http://localhost:8080/` shows recent spans, a filter bar, per-trace waterfalls, and span detail.

**Span identity is a primary key.** `(trace_id, span_id)` uniquely names a span; re-ingesting it replaces the stored version (last write wins). Client retries are idempotent and never create duplicates.

## LLM and agent observability

Traza treats generative-AI telemetry as a native workload, not an attribute soup. The [conventions](docs/llm-semantics.md) are plain span attributes — no SDK required:

```jsonc
{
  "name": "llm.completion",
  "service": "support-agent",
  "attributes": {
    "session.id": "chat-4711",          // groups traces into a session
    "llm.model": "gpt-5.6",
    "llm.prompt_tokens": 412,
    "llm.completion_tokens": 88,
    "llm.cost_usd": 0.0042,
    "llm.stop_reason": "end_turn"
  },
  "events": [{ "name": "llm.prompt", "attributes": { "content": "..." } }]
}
```

**Sessions** — the unit of agent work that spans many traces:

```sh
curl 'http://localhost:8080/v1/sessions?limit=20'
# per session: span/trace counts, token totals, cost, errors, activity window
curl 'http://localhost:8080/v1/sessions/chat-4711'
# adds the per-trace breakdown
```

**Cost and token analytics** — exact rollups, grouped how you ask:

```sh
curl 'http://localhost:8080/v1/stats/llm?group_by=model'     # or service | session | day
# rows of {key, llm_calls, prompt/completion/total tokens, cost_usd, errors, latency}
```

**Prompt and completion payloads** stay queryable without bloating the store: string values above `--payload-threshold-bytes` (default 256 KiB) are offloaded to a content-addressed store and replaced inline by `{"$payload": "sha256/…", "bytes": N, "preview": "…"}`. Identical payloads — repeated system prompts — are stored once. `GET /v1/payloads/{ref}` returns the bytes.

**Evals and feedback** attach to spans after the fact, without mutating them:

```sh
curl -X POST http://localhost:8080/v1/annotations \
  -d '{"trace_id": "trace-1", "span_id": "span-1", "name": "groundedness",
       "value": 0.9, "source": "eval:nightly"}'
```

**Dataset export** turns any search into training/eval data:

```sh
curl 'http://localhost:8080/v1/export?service=support-agent&attr.llm.model=gpt-5.6' > dataset.ndjson
```

Exports stream with bounded memory — an export larger than RAM is fine.
Successful streams end with `X-Traza-Export-Complete: true` and
`X-Traza-Export-Count` HTTP trailers. Programmatic clients must verify the
completion trailer; a `false` value means the stream failed after its `200`
response began.

## OpenTelemetry

Point any OTel SDK at Traza:

```sh
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:8080
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf   # or http/json
export OTEL_EXPORTER_OTLP_COMPRESSION=none
```

`POST /v1/traces` accepts OTLP/HTTP as binary protobuf or JSON: `service.name` becomes the span's service, typed attributes are flattened, events and span links are preserved. gRPC is not served — use the `http/protobuf` exporter setting, which every OTel SDK supports.

## HTTP API

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/v1/spans` | Ingest a JSON span batch; responds `{"accepted": N}` |
| `POST` | `/v1/traces` | OTLP/HTTP ingest (protobuf or JSON) |
| `GET` | `/v1/traces/{trace_id}` | One trace's spans (sorted) plus its annotations |
| `GET` | `/v1/spans?…` | Filtered span search |
| `GET` | `/v1/sessions?since=&until=&limit=` | Sessions, most recent first |
| `GET` | `/v1/sessions/{id}` | One session's rollup + per-trace breakdown |
| `GET` | `/v1/stats/llm?group_by=model\|service\|session\|day` | Token/cost aggregation |
| `POST` | `/v1/annotations` | Attach a score/feedback record to a span or trace |
| `GET` | `/v1/annotations?trace_id=&span_id=&name=` | Query annotations |
| `GET` | `/v1/payloads/sha256/{hex}` | Raw bytes of an offloaded payload |
| `GET` | `/v1/export?…` | Chunked NDJSON export with completion/count trailers |
| `GET` | `/v1/stats` | Physical record count, segment count, bytes on disk |
| `POST` | `/v1/flush` | Force buffered spans into a durable segment |

Search filters (ANDed; URL-encoded): `service` and `name` (exact match), `attr.KEY` (exact attribute match — bare values match strings, JSON literals match typed values; repeatable), `min_duration_ms`, `since`/`until` (Unix nanoseconds, inclusive), `limit` (default 100 on `/v1/spans`; exports are unbounded by default).

Timestamps are integer Unix nanoseconds; `start_time_unix_nano` and the aliases `start_timestamp_ns`, `start_ns`, `start_time` are accepted (same for `end_*`). Unknown fields on a span are stored and returned verbatim. Invalid JSON is `400`; bodies are capped at 64 MiB; `503` means retry with backoff.

`/v1/stats` counts physical records, including immutable historical versions
that last-write-wins reads hide until compaction. Its response includes
`record_count`, `buffered_records`, `persisted_records`, `segment_count`, and
`bytes_on_disk`.

## Operating Traza

**Authentication.** Set `TRAZA_TOKENS` to require bearer tokens, with per-token scope — `ro` tokens may GET, `rw` tokens may GET and POST:

```sh
TRAZA_TOKENS="rw:$(openssl rand -hex 16),ro:$(openssl rand -hex 16)" \
  traza-server --data-dir ./data
```

Missing or unknown tokens get 401 with a `WWW-Authenticate: Bearer` challenge; insufficient scope gets 403. Comparison is constant-time; an invalid `TRAZA_TOKENS` refuses startup rather than silently running open. Without tokens, Traza permits loopback binds only; a non-loopback `--host` requires `TRAZA_TOKENS` or the explicit `--allow-unauthenticated-non-loopback` escape hatch. The dashboard shell itself stays open (it carries no data) and prompts for a token on the first 401, holding it in `sessionStorage` only. TLS is reverse-proxy territory.

**Retention.** `--ttl-seconds N` keeps a rolling window: a background pass compacts expired spans every minute, and annotations and payload files age out on the same window. Off by default — nothing is deleted unless you ask.

**Durability.** Durability begins at segment flush: a crash loses at most the unflushed write buffer (bounded by `--flush-spans`, default 10,000), never a completed flush. `POST /v1/flush` narrows the window on demand. Every compaction rewrite is journaled before it begins; recovery finishes an interrupted rewrite in whichever direction the crash left it.

**Resources.** Memory is O(indexes), not O(data): segments are file-backed, only their parsed indexes stay resident, and span payloads are read on demand — measured 0.71 GB peak RSS serving a 10M-span (2.4 GB on disk) corpus. Stores larger than RAM serve correctly; disk latency applies to cold reads.

**Scope.** One writer process per data directory (a stale lock from a dead process is reclaimed automatically). Single-node today; clustered HA is the designed next arc — see the [roadmap](#status-and-roadmap).

### Server flags

| Flag | Default | Description |
|---|---|---|
| `--data-dir DIR` | `./data` | Directory for all state; created if missing |
| `--host ADDR` / `--port PORT` | `127.0.0.1` / `8080` | Bind address and port |
| `--workers N` | CPUs (min 4) | HTTP worker threads |
| `--ttl-seconds N` | off | Rolling retention window |
| `--flush-spans N` | `10000` | Buffered spans that trigger a durable flush |
| `--payload-threshold-bytes N` | `262144` | Offload threshold for large string values; `0` disables |
| `--allow-unauthenticated-non-loopback` | off | Explicitly allow an unsafe non-loopback bind without tokens |

## Performance

Measured on macOS/aarch64 (10 hardware threads) by the bundled end-to-end benchmark over a 1,000,000-span corpus ingested through the real HTTP path:

- **Sustained ingest:** 116,618 spans/s (batched HTTP, client serialization and loopback included)
- **Trace lookup:** p95 0.64 ms
- **Attribute-filtered search:** p95 3.3 ms

Limited queries decode only the records they return, so lookup and search latency are effectively scale-independent: at 10M spans, trace lookup is p50 0.45 ms and the attribute filter p50 2.9 ms. Full percentiles and methodology are in [BENCHMARKS.md](BENCHMARKS.md), which is rewritten by the benchmark itself (`cargo run --release --bin bench`) — run it on your hardware rather than trusting ours.

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

## Design

Two layers with one contract: never lose a completed write, never serve a torn one.

The **storage engine** buffers spans in memory and flushes sorted, immutable v2 segment files — JSON payloads with embedded record-offset, trace, and attribute indexes — via write-temp, fsync, atomic rename. Opening a store parses only the indexes; spans materialize on demand. Legacy v1 JSONL segments fail startup with a migration pointer rather than being silently misread. Filters narrow candidates through the indexes, then re-verify every predicate against the parsed span: an index accelerates a filter, it never changes its semantics.

The **HTTP server** is a deliberately small HTTP/1.1 implementation on `std::net` — a bounded worker pool in front of the engine, which is its only datastore. There is no server-side log or side index; restart durability is the engine's.

Deeper reading: [segment format](docs/segment-format-v2.md) · [LLM conventions](docs/llm-semantics.md) · [HA design](docs/ha-design.md).

## Status and roadmap

Traza is pre-1.0 and honest about it: on-disk formats may change between 0.x versions, and single-node is the current scope. Shipped and load-bearing today: durable segment storage with crash recovery, OTLP protobuf/JSON ingest, sessions and cost analytics, payload offloading, annotations, streaming export, bearer auth, and the bundled dashboard.

The destination is bigger than one node. The full product roadmap — from production-ready single node (1.0) through replicated HA clusters, columnar analytics at billion-span scale, agent-native debugging depth, and enterprise operation — lives in [docs/roadmap.md](docs/roadmap.md), with the HA architecture detailed in [docs/ha-design.md](docs/ha-design.md). Same binary, same API, at every phase.

Deliberately out of scope: a metrics/logs suite, embedded eval models, general SQL, and framework SDKs — the [roadmap](docs/roadmap.md#explicit-non-goals) explains why.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). The short version: stable Rust is the only dependency, `./ci.sh` is the merge bar, and new dependencies need a reason.

## License

Licensed under the [Apache License, Version 2.0](LICENSE). Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be licensed as above, without any additional terms or conditions.
