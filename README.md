# Traza

**A trace datastore with first-class LLM and agent observability — one binary from laptop to cluster.**

Traza (Spanish for "trace") ingests OpenTelemetry or plain-JSON spans over HTTP, stores them durably, and answers trace lookups, filtered searches, and token/cost analytics in milliseconds — with a trace browser and no infrastructure to stand up. Two dependencies (`serde`, `serde_json`), no external database, and one deployment story at every size: today a single node that starts in milliseconds; the designed trajectory is replicated, highly available clusters of the same binary. The trace browser is a React app ([`ui/`](ui/)) that the server serves straight from its build output — nothing is compiled into the binary, so building the server still needs no Node toolchain.

```sh
cargo build --release
(cd ui && npm ci && npm run build)          # builds the dashboard into ui/dist
./target/release/traza-server --data-dir ./data --port 8080
# open http://localhost:8080 — the server serves ui/dist
```

**The dashboard is a build artifact, not part of the binary.** `traza-server`
compiles with no Node toolchain and embeds no HTML; it serves whatever built
dashboard it finds on disk. So a `cargo install`ed or otherwise packaged
binary ships the **API only** — until you give it a build. It looks in, in
order: `--ui-dir`, `$TRAZA_UI_DIR`, `<directory of the binary>/ui`,
`<binary>/../share/traza/ui`, then `./ui/dist`. Packagers should drop the
build beside the executable as `ui/`; from a checkout, `npm run build` puts it
at `./ui/dist`. Without any of them the API runs exactly the same, `/` returns
a 404 saying how to build it, and startup logs every path it searched.

## Why Traza

Most tracing backends make you assemble a fleet before the first span: a column store or search cluster, a queue in front, an operator keeping it healthy. Traza's bet is that a trace datastore should scale like a database, not like a pipeline — **one binary whose deployment grows with you** instead of a different architecture at every size:

- **Start on one machine in seconds.** A single process stores everything under `--data-dir` and needs nothing else — right-sized for a laptop, a CI job, a single-host service, an edge box, or the AI agent you're debugging *right now*.
- **Scale by adding nodes, not systems.** The engine's foundations — immutable segments, idempotent primary-key ingest, journaled compaction — were chosen to replicate. The [HA design](docs/ha-design.md) (quorum-replicated logical log, validated full-state snapshots for catch-up) is the committed trajectory: clustered, highly available deployments running this same binary. Today's scope is single-node; see [Status](#status-and-roadmap).
- **Built for LLM and agent workloads.** Sessions, token and cost analytics, prompt/completion capture with large-payload offloading, post-hoc evals and feedback, and one-command dataset export — first-class, not bolted on.
- **OpenTelemetry-compatible, OpenLLMetry-native.** Point any OTel SDK at it with two environment variables. Traza follows the [OpenLLMetry](https://github.com/traceloop/openllmetry) standard (`gen_ai.*` / `traceloop.*`), so instrumented apps get sessions and token/cost analytics with no attribute renaming.
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

Or skip curl entirely: the trace browser at `http://localhost:8080/` shows recent spans, a filter bar, per-trace waterfalls, span detail, sessions, and LLM analytics. It is a React app in [`ui/`](ui/); `npm run build` emits `ui/dist`, which the server serves (see [`--ui-dir`](#server-flags)). For UI development, `npm run dev` runs Vite on :5173 with `/v1` proxied to the server.

**Span identity is a primary key.** `(trace_id, span_id)` uniquely names a span; re-ingesting it replaces the stored version (last write wins). Client retries are idempotent and never create duplicates.

## LLM and agent observability

Traza treats generative-AI telemetry as a native workload, not an attribute
soup. It follows the [OpenLLMetry](https://github.com/traceloop/openllmetry)
standard — Traceloop's OpenTelemetry GenAI conventions — so an app instrumented
with OpenLLMetry populates sessions and cost/token analytics with no attribute
renaming. Traza's own shorthand (`llm.*`, `session.id`) is accepted as an
alias. The [conventions](docs/llm-semantics.md) are plain span attributes — no
SDK required:

```jsonc
{
  "name": "openai.chat",
  "service": "support-agent",
  "attributes": {
    "gen_ai.conversation.id": "chat-4711",   // groups traces into a session
    "gen_ai.provider.name": "openai",         // provider
    "gen_ai.operation.name": "chat",
    "gen_ai.request.model": "gpt-4o",
    "gen_ai.usage.input_tokens": 412,
    "gen_ai.usage.output_tokens": 88,
    "gen_ai.response.finish_reason": "stop",
    // messages: JSON gen_ai.input.messages / gen_ai.output.messages
    "gen_ai.input.messages": "[{\"role\":\"user\",\"parts\":[{\"type\":\"text\",\"content\":\"...\"}]}]"
  }
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
curl 'http://localhost:8080/v1/stats/llm?group_by=model'     # or provider | service | session | day
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

OpenLLMetry and OTel GenAI instrumentation lands queryable without translation: `gen_ai.*`, `llm.usage.*`, and `traceloop.*` attributes drive sessions, provider/model rollups, and token/cost analytics directly (see [LLM conventions](docs/llm-semantics.md)).

## HTTP API

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/v1/spans` | Ingest a JSON span batch; responds `{"accepted": N, "durability": MODE}` |
| `POST` | `/v1/traces` | OTLP/HTTP ingest (protobuf or JSON) |
| `GET` | `/v1/traces/{trace_id}` | One trace's spans (sorted) plus its annotations |
| `GET` | `/v1/spans?…` | Filtered span search |
| `GET` | `/v1/sessions?since=&until=&limit=` | Sessions, most recent first |
| `GET` | `/v1/sessions/{id}` | One session's rollup + per-trace breakdown |
| `GET` | `/v1/stats/llm?group_by=model\|provider\|service\|session\|day` | Token/cost aggregation |
| `POST` | `/v1/annotations` | Attach a score/feedback record to a span or trace |
| `GET` | `/v1/annotations?trace_id=&span_id=&name=` | Query annotations |
| `GET` | `/v1/payloads/sha256/{hex}` | Raw bytes of an offloaded payload |
| `GET` | `/v1/export?…` | Chunked NDJSON export with completion/count trailers |
| `GET` | `/v1/stats` | Physical record count, segment count, bytes on disk |
| `GET` | `/v1/metrics` | Prometheus text: per-stage ingest timings, request and connection counters |
| `POST` | `/v1/flush` | Force buffered spans into a durable segment |

Search filters (ANDed; URL-encoded): `service` and `name` (exact match), `attr.KEY` (exact attribute match — bare values match strings, JSON literals match typed values; repeatable), `session` (all spans of a session, unioning every recognized session key), `min_duration_ms`, `since`/`until` (Unix nanoseconds, inclusive), `limit` (default 100 on `/v1/spans`; exports are unbounded by default).

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

Missing or unknown tokens get 401 with a `WWW-Authenticate: Bearer` challenge; insufficient scope gets 403. Comparison is constant-time; an invalid `TRAZA_TOKENS` refuses startup rather than silently running open. Without tokens, Traza permits loopback binds only; a non-loopback `--host` requires `TRAZA_TOKENS` or the explicit `--allow-unauthenticated-non-loopback` escape hatch. The dashboard SHELL itself stays open (it is static build output and carries no data) while every `/v1` call it makes remains gated; the page prompts for a token on the first 401 and holds it in `sessionStorage` only. TLS is reverse-proxy territory.

**Retention.** `--ttl-seconds N` keeps a rolling window: a background pass compacts expired spans every minute, and annotations and payload files age out on the same window. Off by default — nothing is deleted unless you ask.

**Durability.** An acknowledged write means what `--durability` says it means, and the mode is reported in every ingest response and in `/v1/stats` so a client never has to guess:

| Mode | A `200` means | Cost |
|---|---|---|
| `buffered` | accepted in memory; a crash loses anything not yet flushed | fastest, **lossy by design** |
| `wal` (default) | fsynced to the write-ahead log and recovered on restart | one group-committed fsync per batch |
| `flushed` | present in a sealed segment | a segment write per call |

The log is appended and fsynced *before* ingest returns, and replayed into the write buffer on open; once a flush seals those spans into a segment the log is reclaimed. The fsync happens outside the writer lock, so concurrent batches coalesce into one sync — measured 13.7k spans/s at concurrency 1 rising to 48.1k at concurrency 16 on the reference laptop, where a per-batch fsync would stay flat. `buffered` reaches 247k spans/s and `flushed` 6.0k, which is the trade-off the mode names.

Recovery is ordered: log records replay in append order, so a re-ingested span recovers as its newest version, exactly as last-write-wins had it before the crash. A torn or corrupt trailing record is discarded — it was never acknowledged, because the acknowledgement follows the fsync. Annotations and payload writes already fsync on their own path. Every compaction rewrite is journaled before it begins; recovery finishes an interrupted rewrite in whichever direction the crash left it.

One caveat worth stating plainly: `wal` and `flushed` issue `fsync`, which on **macOS does not flush the drive's own write cache** (that needs `F_FULLFSYNC`, which the Rust standard library does not expose and which this crate will not reach for while it has two dependencies and forbids unsafe code). On Linux, `fsync` carries the usual guarantee. A macOS laptop losing power can therefore still lose an acknowledged write; a kill -9, a panic, or an OS crash cannot, and that is what the durability suite proves.

**Resources.** Memory is O(indexes), not O(data): segments are file-backed, only their parsed indexes stay resident, and span payloads are read on demand — measured **0.25 GB peak RSS** serving a 10M-span corpus (**~6 GB on disk** for the benchmark's span shape). Stores larger than RAM serve correctly; disk latency applies to cold reads.

**Scope.** One writer process per data directory (a stale lock from a dead process is reclaimed automatically). Single-node today; clustered HA is the designed next arc — see the [roadmap](#status-and-roadmap).

### Server flags

| Flag | Default | Description |
|---|---|---|
| `--data-dir DIR` | `./data` | Directory for all state; created if missing |
| `--host ADDR` / `--port PORT` | `127.0.0.1` / `8080` | Bind address and port |
| `--max-connections N` | `1024` | Concurrent connections served; past it clients get `503` rather than being queued |
| `--ttl-seconds N` | off | Rolling retention window |
| `--flush-spans N` | `10000` | Buffered spans that trigger a durable flush |
| `--payload-threshold-bytes N` | `262144` | Offload threshold for large string values; `0` disables |
| `--durability MODE` | `wal` | `buffered`, `wal`, or `flushed` — what an acknowledged write guarantees (see [Durability](#operating-traza)) |
| `--compaction-fanout N` | `4` | Same-size segments merged into one, bounding filtered-search cost; `0` disables compaction |
| `--compaction-max-segment-bytes N` | `268435456` | Ceiling on a merged segment, bounding merge memory and lock hold time; `0` for no ceiling |
| `--ui-dir DIR` | see below | Built dashboard to serve at `/`; served from disk, so a rebuilt UI needs no server restart. Unset ⇒ `$TRAZA_UI_DIR`, `<binary dir>/ui`, `<binary dir>/../share/traza/ui`, `./ui/dist`, first one containing `index.html`. None found ⇒ the API runs and `/` 404s with build instructions |
| `--allow-unauthenticated-non-loopback` | off | Explicitly allow an unsafe non-loopback bind without tokens |

## Performance

Measured on macOS/aarch64 (10 hardware threads) by the bundled end-to-end benchmark over a 1,000,000-span corpus ingested through the real HTTP path:

- **Sustained ingest:** 116,618 spans/s (batched HTTP, single client, client serialization and loopback included)
- **Sustained ingest, 16 concurrent clients:** 208,973 spans/s (`wal` durability, median of 5 runs — see [INGEST-BENCHMARK.md](INGEST-BENCHMARK.md))
- **Trace lookup:** p95 0.64 ms
- **Attribute-filtered search:** p95 3.3 ms

Limited queries decode only the records they return, so **trace lookup** is effectively scale-independent — measured p50 0.85 ms, p99 4.65 ms over a 10M-span store.

**Filtered search costs one index probe per segment**, so its latency tracks the number of segments rather than the size of the corpus — which is what [size-tiered compaction](#server-flags) exists to bound. It is on by default. All three columns below come from the same benchmark harness at 100M spans (~55 GB on disk), differing only in `--compaction-fanout` and `--compaction-max-segment-bytes`:

| 100M spans | uncompacted | default compaction (256 MiB cap) | **1 GiB cap** |
|---|---:|---:|---:|
| Attribute filter p50 | 155.5 ms | 9.8 ms | **2.3 ms** |
| Attribute filter p95 | 747.3 ms | 27.1 ms | **9.3 ms** |
| Attribute filter p99 | 1664.6 ms | 72.9 ms | **22.2 ms** |
| Trace lookup p99 | 7.72 ms | 1.82 ms | **0.99 ms** |
| Segments | ~10,100 † | ~380 † | **~100-125** ‡ |
| Peak RSS | 0.43 GB | 2.0 GB | **6.7 GB** ‡ |
| Sustained ingest | 59,025/s | 40,894/s | **31,267/s** |

† Extrapolated from mid-run samples (6,064 at 60M uncompacted, 191 at 50M compacted, both growing linearly); the benchmark deletes its data directory on exit.
‡ Sampled directly during the run at 20-second intervals, not extrapolated. The segment count oscillates between about 97 and 125 over the last 5 minutes of ingest as merges create and retire segments. Peak RSS is the maximum of those samples, so a shorter-lived merge spike between samples could exceed it. Every latency and throughput figure is measured.

Read that honestly. Compaction is worth roughly **16-28x** on filtered search at the default cap, and raising the cap to 1 GiB is worth roughly another **3-4x** on top of that. **At a 1 GiB cap, filtered-search p99 is 22.2 ms at 100M — inside the 50 ms target this project sets itself**, where the 256 MiB default measures 72.9 ms and misses it.

That win is paid for in memory and ingest. Peak RSS rises from 2.0 GB to 6.7 GB, because a merge materializes its inputs and a 4x larger cap means a proportionally larger merge working set; sustained ingest falls a further 24% (40,894/s to 31,267/s). Raising the cap is the right trade for a large store that is read more than it is written, and the wrong one for a memory-constrained host. Both configurations are one flag apart.

This is measured at 100M spans on one machine. It is **not** measured beyond that size, and segment count still grows with the corpus, so the same tail will return at a large enough store — the structural answer remains a per-segment inverted index.

One operational note for the uncompacted path: every segment holds an open file descriptor, so ~10,100 segments means ~10,100 fds. That is fine against a large limit but would exhaust a default 1024-fd shell, which is a second reason not to run large stores with compaction disabled.

Full percentiles and methodology are in [BENCHMARKS.md](BENCHMARKS.md), which is rewritten by the benchmark itself (`cargo run --release --bin bench`) — run it on your hardware rather than trusting ours. Those published figures are the 1,000,000-span corpus; `TRAZA_BENCH_SPANS` runs other sizes without overwriting them.

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

The **HTTP server** is a deliberately small HTTP/1.1 implementation on `std::net` — a bounded worker pool in front of the engine, which is its only datastore. There is no server-side log or side index; restart durability is the engine's. It also serves the dashboard's build directory as static files, resolved against the canonicalized root so no request can read outside it.

Deeper reading: [segment format](docs/segment-format.md) · [LLM conventions](docs/llm-semantics.md) · [HA design](docs/ha-design.md).

## Status and roadmap

Traza is pre-1.0 and honest about it: on-disk formats may change between 0.x versions, and single-node is the current scope. Shipped and load-bearing today: durable segment storage with crash recovery, OTLP protobuf/JSON ingest, sessions and cost analytics, payload offloading, annotations, streaming export, bearer auth, and the [`ui/`](ui/) trace browser served from its build output.

The destination is bigger than one node. The full product roadmap — durable v1 foundations (a write-ahead log and the identity model that must precede any format freeze), then replicated HA clusters and agent-native debugging depth, then columnar analytics at billion-span scale, then the enterprise control plane — lives in [docs/roadmap.md](docs/roadmap.md), with the HA architecture detailed in [docs/ha-design.md](docs/ha-design.md). Same binary, same API, at every phase.

Deliberately out of scope: a metrics/logs suite, embedded eval models, general SQL, and framework SDKs — the [roadmap](docs/roadmap.md#explicit-non-goals) explains why.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). The short version: stable Rust is the only dependency, `./ci.sh` is the merge bar, and new dependencies need a reason.

## License

Licensed under the [Apache License, Version 2.0](LICENSE). Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be licensed as above, without any additional terms or conditions.
