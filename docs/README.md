# Traza documentation

Traza is a trace datastore with first-class LLM and agent observability: one
binary, three dependencies, no infrastructure to stand up. This index routes you
to the right document. Start with the row that matches what you are doing.

| I want to… | Read |
|---|---|
| Get a server running and send my first span | [Getting started](guide/getting-started.md) |
| Understand what a span is and how identity works | [Data model](guide/data-model.md) |
| Send traces from my app or an OTel SDK | [Ingest](guide/ingest.md) |
| Look up an exact route, parameter, or response shape | [HTTP API reference](guide/http-api.md) |
| Query LLM/agent telemetry — sessions, tokens, cost | [LLM semantics](llm-semantics.md) |
| Let a coding agent read the store over MCP | [MCP server](guide/mcp.md) · [demo](../examples/mcp-demo/README.md) |
| Watch the claims proven live before reading further | [The demo tour](../examples/README.md) |
| Navigate the trace browser | [Trace browser](guide/trace-browser.md) |
| Deploy, tune, or operate a server | [Operations](#operators) |
| Change Traza's code | [Internals](#developers) |

## Users

Sending traces to Traza and getting answers back out.

- **[Getting started](guide/getting-started.md)** — build, run, ingest, and see
  a trace in the UI. Every command is copy-pasteable and was run as written.
- **[Data model](guide/data-model.md)** — spans, the `(trace_id, span_id)`
  primary key, last-write-wins and what it means for retries, timestamps,
  attributes, events, and links.
- **[Ingest](guide/ingest.md)** — `POST /v1/spans` (native JSON) versus
  `POST /v1/traces` (OTLP/HTTP), when to reach for which, OTel SDK setup,
  batching, and error handling.
- **[HTTP API reference](guide/http-api.md)** — every route, its parameters,
  its response shape, and its errors.
- **[LLM semantics](llm-semantics.md)** — the OpenLLMetry / OTel GenAI
  attributes Traza recognizes, sessions, token and cost analytics, prompt and
  completion payloads, span links, and query recipes.
- **[MCP server](guide/mcp.md)** — the Model Context Protocol endpoint: its ten
  tools, its resources and prompts, how results are bounded for a context
  window, and the untrusted-content boundary stored span text is held behind.
- **[Trace browser](guide/trace-browser.md)** — what each view shows and how to
  move between them.

## Operators

Deploying, tuning, and keeping it healthy.

- **[Deployment](operations/deployment.md)** — single binary, one writer per
  data directory, what lives on disk, and how the dashboard is served.
- **[Durability](operations/durability.md)** — the three acknowledgement modes
  and precisely what a `200` promises in each, including the platform caveat
  that a macOS `fsync` does not flush the drive's write cache.
- **[Administration](operations/administration.md)** — authentication and
  scopes, retention/TTL, targeted erasure and its receipt, compaction,
  payload offloading, and backups.
- **[Backup and restore](operations/backup.md)** — generations, `CURRENT`, and
  the pin-verify-copy backup that runs without stopping the server.
- **[Monitoring](operations/monitoring.md)** — `GET /v1/metrics` and
  `GET /v1/stats`, what each metric means, and what is worth alerting on.
- **[Capacity and performance](operations/capacity.md)** — measured
  characteristics, with every number traced to the file that recorded it.
- **[Storage comparison](storage-comparison.md)** — bytes stored per byte
  ingested and what it costs, next to OpenObserve's published Elasticsearch
  comparison. Traza loses this one on ordinary spans and wins it on
  long-context agent traffic; both numbers are measured.
- **[Configuration reference](configuration.md)** — the exhaustive
  flag-by-flag reference plus throughput and latency profiles. Flags are
  documented there and nowhere else, so there is one place to correct.

## Developers

Changing Traza's code.

- **[Architecture](internals/architecture.md)** — the two layers, and how a
  write travels from socket to sealed segment.
- **[Invariants](internals/invariants.md)** — the load-bearing rules a change
  must not break. Read this before touching the engine.
- **[Module map](internals/module-map.md)** — what each file in `src/` owns.
- **[Testing](internals/testing.md)** — how the suite is organised, what each
  file covers, and the standard that a test must be shown to fail when the
  behaviour it guards is broken.
- **[Benchmarking](internals/benchmarking.md)** — running `bench`,
  `ingest-bench`, `storage-bench` and `query-bench`, and the rules for
  reporting a measurement honestly.
- **[Segment format](segment-format.md)** — the on-disk layout, byte by byte:
  the shipped v7 format, the v6 → v7 migration contract, and the historical
  v6 layout the migrator's frozen decoder reads.
- **[Dependencies](internals/dependencies.md)** — the standing dependency
  budget, the written case for each dependency taken, and the rejections
  that keep the count where it is.
- **[Contributing](../CONTRIBUTING.md)** — setup, the `./ci.sh` gate, and pull
  request expectations.

## Measurement records

These are written by the benchmarks themselves, not by hand. Documentation
cites them rather than restating numbers.

They live in [`benchmarks/`](benchmarks/); how to run each one, and the rules
for reporting a measurement honestly, are in
[benchmarking](internals/benchmarking.md).

- **[canonical-corpus.md](benchmarks/canonical-corpus.md)** — the canonical
  corpus run, rewritten by `cargo run --release --bin bench`.
- **[ingest.md](benchmarks/ingest.md)** — the ingest matrix over
  protocol, keep-alive, concurrency, and durability, from
  `cargo run --release --bin ingest-bench`.
- **[storage.md](benchmarks/storage.md)** — bytes on disk per byte ingested,
  from `cargo run --release --bin storage-bench`.
- **[query.md](benchmarks/query.md)** — LLM aggregation latency, cold and
  under concurrent ingest, from `cargo run --release --bin query-bench`.
- **[index-memory.md](benchmarks/index-memory.md)** — resident index memory
  and the compaction transient, from `index-mem-bench --matrix`. Its raw
  per-cell results are committed alongside it as
  [`index-memory.json`](benchmarks/index-memory.json), which
  `tests/measurement_records.rs` checks the capacity guide against.
- **[CHANGELOG.md](../CHANGELOG.md)** — what changed, when, and what it
  measured.
