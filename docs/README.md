# Traza documentation

Traza is a trace datastore with first-class LLM and agent observability: one
binary, two dependencies, no infrastructure to stand up. This index routes you
to the right document. Start with the row that matches what you are doing.

| I want to… | Read |
|---|---|
| Get a server running and send my first span | [Getting started](guide/getting-started.md) |
| Understand what a span is and how identity works | [Data model](guide/data-model.md) |
| Send traces from my app or an OTel SDK | [Ingest](guide/ingest.md) |
| Look up an exact route, parameter, or response shape | [HTTP API reference](guide/http-api.md) |
| Query LLM/agent telemetry — sessions, tokens, cost | [LLM semantics](llm-semantics.md) |
| Let a coding agent read the store over MCP | [MCP server](guide/mcp.md) · [demo](../examples/mcp-demo/README.md) |
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
  scopes, retention/TTL, compaction, payload offloading, and backups.
- **[Monitoring](operations/monitoring.md)** — `GET /v1/metrics` and
  `GET /v1/stats`, what each metric means, and what is worth alerting on.
- **[Capacity and performance](operations/capacity.md)** — measured
  characteristics, with every number traced to the file that recorded it.
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
- **[Benchmarking](internals/benchmarking.md)** — running `bench` and
  `ingest-bench`, and the rules for reporting a measurement honestly.
- **[Segment format](segment-format.md)** — the on-disk layout, byte by byte.
- **[Contributing](../CONTRIBUTING.md)** — setup, the `./ci.sh` gate, and pull
  request expectations.

## Project direction

- **[Roadmap](roadmap.md)** — phases, acceptance gates, and explicit non-goals.
- **[Generations and checkpoints](generations-design.md)** — the proposed
  single-node state boundary that would make backup, export, retention and
  replication one mechanism. Design, not shipped behaviour.
- **[High-availability design](ha-design.md)** — the replicated, clustered
  trajectory. Design, not shipped behaviour; today's scope is single-node.

## Measurement records

These are written by the benchmarks themselves, not by hand. Documentation
cites them rather than restating numbers.

- **[BENCHMARKS.md](../BENCHMARKS.md)** — the canonical corpus run, rewritten
  by `cargo run --release --bin bench`.
- **[INGEST-BENCHMARK.md](../INGEST-BENCHMARK.md)** — the ingest matrix over
  protocol, keep-alive, concurrency, and durability, from
  `cargo run --release --bin ingest-bench`.
- **[CHANGELOG.md](../CHANGELOG.md)** — what changed, when, and what it
  measured.
