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
| Stop a runaway agent run before its next step | [Run fuses](guide/guard.md) · [demo](../examples/guard/README.md) · [Claude Code hook pack](../examples/claude-code/README.md) |
| Watch live runs, nudge or veto a running agent, see how its agents hand off | [Steer](guide/steer.md) · [Claude Code hook pack](../examples/claude-code/README.md) |
| Re-run a recorded agent run with zero model calls, or diff two runs | [Replay](guide/replay.md) · [demo](../examples/replay/README.md) |
| See which paths an agent's runs take and how each ends; foresee a live run's fate; stop a run whose path has always failed | [Foresight](guide/foresight.md) · [demo](../examples/foresight/README.md) |
| Record every Claude Code and Codex session with zero setup, or ask which agent step wrote a line of code | [Recorders and Agent Blame](guide/recorders.md) · [demo](../examples/coding-agents/README.md) |
| See exactly what a model saw at one step, where each part came from, and why its prompt cache broke | [Context X-Ray and Cache Autopsy](guide/xray.md) · [demo](../examples/coding-agents/README.md) |
| Find out what changed in an agent — a prompt, a tool, a model, a version — and whether it made things worse | [Change radar](guide/radar.md) |
| Find every run whose steps did something — like a regex over behaviour — or run the built-in detectors | [TQL and detectors](guide/tql.md) · [demo](../examples/coding-agents/README.md) |
| Branch a recorded run at any step, edit an answer and go live; test agents against golden runs | [Fork and rewind](guide/fork.md) |
| Prove what the store committed, for an auditor or a notary | [Ledger and evidence packs](guide/ledger.md) · [demo](../examples/prove/README.md) |
| Call the guard, replays, diff, recall or the ledger from Python or TypeScript | [Client libraries](guide/clients.md) |
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
- **[MCP server](guide/mcp.md)** — the Model Context Protocol endpoint: its
  twenty-four tools, its resources and prompts, how results are bounded for a
  context window, and the untrusted-content boundary stored span text is held
  behind.
- **[Run fuses](guide/guard.md)** — per-run limits on cost, tokens, steps,
  repeated calls and failure streaks; the pre-call check, the verdict on every
  ingest acknowledgement, the kill switch, and why a post-ack stop always
  lands one step late.
- **[Foresight](guide/foresight.md)** — the Fate Map: every path an agent's
  runs took, each branch's success rate with a Wilson interval, the fatal
  forks and hotspots, the map's own calibration; foresight for one run with
  its verdict and what earlier runs did next; and predictive fuses that stop a
  run whose path has historically failed.
- **[Steer](guide/steer.md)** — the runs live now with their foresight,
  nudging or vetoing a running agent before its next step, how a run's agents
  handed work around, and a fuse limit on agents ping-ponging.
- **[Replay](guide/replay.md)** — a run read as a tape, replaying it through
  the OpenAI or Anthropic SDK with zero model calls, diffing two runs, and
  recalling earlier runs that did the same thing.
- **[Ledger and evidence packs](guide/ledger.md)** — the hash chain over
  every published generation and erasure, verifying it with or without the
  server, external anchors, and the evidence pack a notary signs; with what
  it proves and what it does not.
- **[Recorders and Agent Blame](guide/recorders.md)** — `traza-server import
  claude-code|codex` turns coding-agent transcripts into runs (live with
  `--watch`), and `traza-server blame` names the step, prompt and model that
  introduced a line of code.
- **[Context X-Ray and Cache Autopsy](guide/xray.md)** — one model call's
  context window as chunks, with provenance, stale tool results, duplicates
  and compaction loss; and why each call's prompt cache held or broke, the
  volatile token named and the loss priced.
- **[Change radar](guide/radar.md)** — change points in each agent's system
  prompt, tools, models, parameters and versions, and shifts in its behaviour
  with no configuration change, each with before/after deltas, p-values and
  a verdict.
- **[TQL and detectors](guide/tql.md)** — a pattern language over a run's
  steps, compiled to an NFA, and the built-in detector library (test
  tampering, give-up loops, edit thrash, ping-pong and more).
- **[Fork and rewind](guide/fork.md)** — a replay that serves edited answers
  and goes live after a chosen step, the forks of a run diffed against it,
  golden runs and tape tests from Python and TypeScript.
- **[Client libraries](guide/clients.md)** — the zero-dependency Python and
  TypeScript clients for the routes an agent harness calls: the pre-call
  guard and the rest of the run fuses, tapes and replays, diffs, recall, the
  ledger head and evidence packs, and the OTLP exporter environment. Typed
  clients, not instrumentation.
- **[Trace browser](guide/trace-browser.md)** — what each view shows and how to
  move between them.

## Operators

Deploying, tuning, and keeping it healthy.

- **[Deployment](operations/deployment.md)** — single binary, one writer per
  data directory, what lives on disk, and how the dashboard is served.
- **[Durability](operations/durability.md)** — the three acknowledgement modes
  and precisely what a `200` promises in each, including the platform caveat
  that a macOS `fsync` does not flush the drive's write cache.
- **[Administration](operations/administration.md)** — authentication,
  scopes and runtime credentials, tenant quotas and usage, retention/TTL,
  targeted erasure and its receipt, compaction, payload offloading, and
  backups.
- **[Backup and restore](operations/backup.md)** — generations, `CURRENT`, and
  the pin-verify-copy backup that runs without stopping the server.
- **[Object-storage archive (preview)](operations/object-storage.md)** —
  publish a verified pin to S3-compatible storage as an immutable snapshot
  that is queryable by range reads, restorable, and deletable; behind the
  `object-storage` cargo feature.
- **[Monitoring](operations/monitoring.md)** — `GET /v1/metrics` and
  `GET /v1/stats`, what each metric means, and what is worth alerting on.
- **[Capacity and performance](operations/capacity.md)** — measured
  characteristics, with every number traced to the file that recorded it.
- **[Storage comparison](storage-comparison.md)** — bytes stored per byte
  ingested and what it costs, next to OpenObserve's published Elasticsearch
  comparison. Traza stores less than it is sent on every corpus; the HA cost
  row still belongs to object-storage economics on ordinary spans, and
  long-context agent traffic wins outright. Every Traza number is measured;
  the Elasticsearch and OpenObserve halves are their published figures.
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
  the shipped v8 format, the v6-and-v7 → v8 migration contract, and the
  historical v7 and v6 layouts the migrator's frozen decoders read.
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
- **[storage-v8-comparison.md](benchmarks/storage-v8-comparison.md)** — the
  one hand-written page in this directory: the v7 → v8 before/after,
  transcribing paired same-machine runs of the harnesses above that the
  latest-run-only generated records cannot show.
- **[CHANGELOG.md](../CHANGELOG.md)** — what changed, when, and what it
  measured.
