# Traza

*Traza* (Spanish for "trace") is a lightweight, single-binary tracing datastore written in Rust.

It accepts batches of spans over a plain JSON HTTP API, persists them to an append-only store on local disk, and answers trace lookups and filtered span queries in milliseconds — no external database, no runtime, and exactly two dependencies (`serde` and `serde_json`).

```sh
cargo build --release
./target/release/traza-server --data-dir ./data --port 8080
```

## Why Traza

Most tracing backends assume a fleet: a column store or search cluster to run, a queue in front of it, an operator to keep it healthy. That is the right trade for a large organization and the wrong one for a laptop, a CI job, a single-host service, or an edge box.

Traza takes the other side of the trade:

- **One process, one directory.** The server starts in milliseconds and stores everything under `--data-dir`.
- **A small audit surface.** Two direct dependencies; networking, threading, file I/O, and HTTP parsing use the Rust standard library.
- **Honest numbers.** Every figure in [BENCHMARKS.md](BENCHMARKS.md) is measured by a bundled end-to-end benchmark, never estimated.
- **Crash-safe by construction.** Immutable segments written by write-temp, fsync, atomic rename; recovery loads only complete segments and heals crash artifacts.

## Features

- Batched span ingestion over HTTP with bounded queues and backpressure.
- Trace-by-ID lookup and filtered span search: service, operation name, exact attribute match, minimum duration, and time window, combined with logical AND.
- An embeddable Rust library (`traza::Store`): an append-only segment storage engine with sorted-batch flush, crash recovery, and TTL compaction.
- A self-verifying benchmark binary that measures the real HTTP path and rewrites `BENCHMARKS.md` from its own run.
- Dual-licensed under MIT or Apache-2.0.

## Performance

Measured by `cargo run --release --bin bench` against a 1,000,000-span corpus (100,000 traces, 20 services, 100 indexed attribute values) on macOS/aarch64 with 10 hardware threads:

| Metric | Measured | Target | Result |
|---|---:|---:|---|
| Sustained batched HTTP ingest | 116,618 spans/s | >= 50,000 spans/s | PASS |
| Trace-by-id p95 | 0.642 ms | < 50 ms | PASS |
| Attribute-filtered query p95 | 3.344 ms | < 300 ms | PASS |

**What these figures measure:** the engine-backed path with format-v2 indexed segments — ingest passes through the write buffer and segment encoder, trace lookups binary-search each segment's embedded trace index, and filters merge undecoded index postings across segments in start-time order, decoding and re-verifying only the records a limited query actually returns. At a 10M-span corpus (10x the canonical run), measured trace lookup is p50 0.45 ms and the attribute filter p50 2.9 ms — both effectively scale-independent for limited queries. The ingest rate is timed over the full loop — client-side JSON serialization and loopback HTTP overhead included. Full percentiles and methodology live in [BENCHMARKS.md](BENCHMARKS.md). Results are machine-specific; run the benchmark yourself (see below) rather than treating these as guarantees.

## Quickstart

Build and start the server:

```sh
cargo build --release
./target/release/traza-server --data-dir ./data --port 8080
```

Ingest a batch of spans (the body is a JSON array, or `{"spans": [...]}`):

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

Fetch a whole trace, spans sorted by start time:

```sh
curl http://localhost:8080/v1/traces/trace-1
# {"trace_id":"trace-1","spans":[...]}
```

Search spans with filters:

```sh
curl 'http://localhost:8080/v1/spans?service=checkout&attr.region=us-east&min_duration_ms=2&limit=50'
```

Check what the store is holding:

```sh
curl http://localhost:8080/v1/stats
# {"span_count":1,"segment_count":1,"bytes_on_disk":231}
```

## HTTP API

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/v1/spans` | Ingest a JSON batch of spans; responds `{"accepted": N}` |
| `GET` | `/v1/traces/{trace_id}` | All spans of one trace, sorted by start time; `404` if unknown |
| `GET` | `/v1/spans?...` | Filtered span search |
| `GET` | `/v1/stats` | Span count, segment count, bytes on disk |

Search filters (all supplied predicates are ANDed; values must be URL-encoded):

- `service` — exact service-name match.
- `name` — exact operation-name match.
- `attr.KEY` — exact attribute match for `KEY`; repeat for multiple attributes. Bare values match string attributes; JSON literals (`true`, `3`) match typed values.
- `min_duration_ms` — minimum span duration in milliseconds.
- `since` / `until` — inclusive start-timestamp bounds, Unix nanoseconds.
- `limit` — maximum spans returned (default 100), applied after filtering, sorted by start time.

**Span identity is the (trace_id, span_id) pair, enforced as a primary key**: re-ingesting an existing pair replaces the stored span (last write wins), so client retries are idempotent and never create duplicate copies. Span timestamps are integer Unix nanoseconds. The server reads `start_time_unix_nano` (and the aliases `start_timestamp_ns`, `start_ns`, `start_time`) plus the matching `end_*` keys; any other fields you send are stored and returned verbatim. Invalid JSON gets `400`; requests are capped at 64 MiB; `503` means the ingest writer is unavailable and the batch should be retried with backoff.

## Architecture

Traza is two layers that share one goal: never lose a completed write, never serve a torn one.

**Storage engine (the `traza` library).** Spans accumulate in an in-memory write buffer. At the flush threshold (default 10,000 spans) the batch is sorted and written as an immutable format-v2 segment file — JSON span payloads with an embedded record-offset index, trace index, and attribute index — via write-temp, fsync, atomic rename: a segment is either completely present or not present at all. Opening a store loads segment BYTES and parses only the indexes; spans materialize on demand for the records a query returns. `get_trace` binary-searches each segment's trace index; filters narrow candidates through the service/name/attribute indexes or a time range, then re-verify every predicate against the parsed span — an index accelerates a filter, it never changes its semantics. Pre-v2 JSON-lines segments remain readable beside v2 files (they stay memory-resident until a TTL rewrite upgrades them). There is no cross-segment manifest yet: TTL compaction is atomic per segment file, and recovery heals a crash-duplicated segment on the next open by dropping exact-duplicate spans and deleting fully-duplicate files.

**HTTP server (`traza-server`).** A deliberately small HTTP/1.1 implementation on `std::net`: a worker pool for connections in front of the segment engine, which is the server's only datastore. Every ingest goes through the engine's write buffer and flush/segment machinery; every trace, query, and stats read comes out of the engine's combined buffered-plus-persisted snapshot. There is no server-side log, index, or replay — restart durability and crash recovery are the engine's, and `POST /v1/flush` forces buffered spans into a durable segment on demand. A bounded connection queue throttles producers instead of growing without limit. If a server process dies without cleanup, the next open reclaims the engine's directory lock once the recorded owner process is verifiably gone.

Embedding the engine in your own process:

```rust
use traza::{Config, SpanFilter, Store};

let store = Store::open("./data", Config::default())?;
store.ingest(span)?;          // buffered; flushes automatically at the threshold
store.flush()?;               // or force a durable segment now
let slow_spans = store.query(&SpanFilter {
    service: Some("checkout".into()),
    min_duration_ns: Some(5_000_000),
    ..SpanFilter::default()
})?;
```

## Configuration

`traza-server` command line:

| Flag | Default | Description |
|---|---|---|
| `--data-dir DIR` | `./data` | Directory for the store's files; created if missing |
| `--port PORT` | `8080` | TCP port; the server binds `0.0.0.0` |
| `--workers N` | number of CPUs (min 4) | HTTP worker threads |
| `--ttl-seconds N` | off | Engine TTL retention window; a background thread compacts expired spans every minute. `0` disables |
| `--host ADDR` | `0.0.0.0` | Bind address |
| `--flush-spans N` | `10000` | Engine flush threshold |

Library `Config`:

| Field | Default | Description |
|---|---|---|
| `flush_spans` | `10_000` | Buffered spans that trigger a sorted segment flush |
| `ttl_seconds` | `None` | Retention window for `compact_expired()`; `None` and `Some(0)` both disable expiration |

## Running the benchmark

```sh
cargo run --release --bin bench
```

The benchmark builds the release server, starts it on a free loopback port with a fresh temporary data directory, ingests 1,000,000 spans over HTTP in 1,000-span batches, measures sustained ingest throughput and p50/p95/p99 query latencies, and rewrites `BENCHMARKS.md` with the measurements from that run. Never edit `BENCHMARKS.md` by hand — regenerate it.

Run the test suite and all lint gates with:

```sh
./ci.sh
```

## Limitations

Traza is a v0.1 project and says so plainly:

- **Single-node.** One writer process per data directory; no replication, clustering, or failover yet.
- **No authentication.** No auth, authorization, or TLS — run it on a trusted network or behind a reverse proxy that terminates TLS and enforces access.
- **JSON-only.** No OTLP endpoint yet; ingestion is the JSON HTTP API described above.
- **Durability boundary.** Durability begins at segment flush: a crash loses at most the engine's unflushed write buffer (bounded by `--flush-spans`), never a completed flush. `POST /v1/flush` narrows the window on demand; per-request fsync is deliberately not offered yet.
- **RAM is O(indexes).** Segments are file-backed: only the parsed indexes stay resident, and record payloads are read on demand — measured 0.71 GB peak server RSS over a 10M-span (2.4 GB on disk) corpus. Stores larger than RAM serve correctly; disk latency applies to cold reads.
- **Minimal HTTP.** `Connection: close` per request, no keep-alive, no TLS, 64 MiB body cap.
- **Exact-match filtering.** No full-text search, aggregations, or query language.
- **Compaction crash windows.** Every compaction rewrite is journaled with a supersede marker before it begins; recovery finishes an interrupted rewrite in whichever direction the crash left it, never guessing from content — legitimately re-ingested identical spans always keep their acknowledged cardinality.
- **Unstable formats.** Segment and log layouts may change between 0.x versions.

All of these are on the roadmap, not swept under it.

## Roadmap

- **Streaming results** — chunked HTTP responses for very large result sets.
- **Filter throughput at scale** — posting-list intersection and parse-avoidance for large unlimited result sets (limited queries already decode only what they return).
- **OTLP ingest** — accept OpenTelemetry OTLP alongside the JSON API.
- **LLM-observability semantics** — first-class conventions for prompts, completions, token usage, and tool calls.
- **Auth** — token authentication and per-token authorization.
- **High availability** — replication and failover beyond a single node.
- **Dashboard** — a bundled UI for browsing traces.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). The short version: stable Rust is the only dependency, `./ci.sh` is the merge bar, and new dependencies need a reason.

## License

Licensed under either of the [MIT license](LICENSE-MIT) or the [Apache License, Version 2.0](LICENSE-APACHE), at your option. Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you shall be dual-licensed as above, without any additional terms or conditions.
