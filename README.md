# Traza

[![CI](https://github.com/toshish/traza/actions/workflows/ci.yml/badge.svg)](https://github.com/toshish/traza/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/traza)](https://crates.io/crates/traza)
[![docs.rs](https://img.shields.io/docsrs/traza)](https://docs.rs/traza)
[![MSRV](https://img.shields.io/badge/MSRV-1.75-blue)](Cargo.toml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)

**A trace datastore with first-class LLM and agent observability — one binary from laptop to cluster.**

Traza (Spanish for "trace") ingests OpenTelemetry or plain-JSON spans over HTTP, stores them durably, and answers trace lookups, filtered search, and token/cost analytics in milliseconds — with a trace browser built in and nothing else to stand up. Two dependencies (`serde`, `serde_json`), no external database, `#![forbid(unsafe_code)]`.

![An agent swarm in the trace browser: a 14-span waterfall with the critical path marked, per-span model and token counts, and the run's duration, tokens, and cost at the top.](docs/assets/trace-waterfall.png)

## Install

**Download** — the server with the dashboard already built. No Rust, no Node:

```sh
curl -LO https://github.com/toshish/traza/releases/download/v0.22.1/traza-0.22.1-macos-aarch64.tar.gz
tar xzf traza-0.22.1-macos-aarch64.tar.gz && cd traza-0.22.1-macos-aarch64
./traza-server --data-dir ./data --port 8080
```

`linux-x86_64` and `linux-aarch64` archives are named likewise, musl-static so any distribution works. Archives carry `SHA256SUMS`, provenance attestations (`gh attestation verify traza-*.tar.gz --repo toshish/traza`), and `THIRD_PARTY_NOTICES.md`. macOS quarantines browser downloads of unsigned binaries — `curl` avoids the flag, and `xattr -d com.apple.quarantine traza-server` clears it otherwise.

**Docker** — `FROM scratch`, runs as uid 65534, refuses a non-loopback bind without a token, so mint one you keep:

```sh
TOKEN="rw:$(openssl rand -hex 16)"
echo "$TOKEN"    # the dashboard and API will ask for this
docker run -p 8080:8080 -v traza-data:/data \
  -e TRAZA_TOKENS="$TOKEN" ghcr.io/toshish/traza:v0.22.1
```

**crates.io** — `cargo install traza --locked --bin traza-server` for the server (API only; the dashboard ships in the archives or builds from [`ui/`](ui/)), or `cargo add traza` to [embed the engine](docs/guide/ingest.md#using-the-engine-directly) in your own process.

**Source** — stable Rust ≥ 1.75, plus Node ≥ 22 if you want the dashboard:

```sh
cargo build --release && (cd ui && npm ci && npm run build)
./target/release/traza-server --data-dir ./data --port 8080
```

## First trace in thirty seconds

```sh
curl -X POST http://localhost:8080/v1/spans -H 'Content-Type: application/json' \
  -d '[{"trace_id":"trace-1","span_id":"span-1","name":"charge","service":"checkout",
        "start_time_unix_nano":1700000000000000000,
        "end_time_unix_nano":1700000000002500000,
        "status":"ok","attributes":{"region":"us-east"}}]'
# {"accepted":1,"durability":"wal"}

curl http://localhost:8080/v1/traces/trace-1
# open http://localhost:8080 for the trace browser
```

Any OpenTelemetry SDK exports to Traza with two environment variables (`OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf`), and apps instrumented with [OpenLLMetry](https://github.com/traceloop/openllmetry) or the OTel GenAI conventions land with sessions and token/cost analytics populated, no renaming. Full walkthrough, including the seeded demo corpus: **[Getting started](docs/guide/getting-started.md)**.

## Why Traza

- **One binary whose deployment grows with you.** A single process stores everything under `--data-dir` — laptop, CI job, edge box, or the agent you are debugging right now. The engine's foundations (immutable segments, idempotent primary-key ingest, journaled compaction) were chosen to replicate; the [HA design](docs/ha-design.md) is the committed trajectory, and today's scope is honestly single-node.
- **Built for LLM and agent workloads.** Sessions, token and cost rollups, prompt/completion capture with content-addressed offloading, post-hoc evals and annotations, content search over prompts, live tail, and one-command NDJSON export — first-class, not bolted on. See [LLM semantics](docs/llm-semantics.md).
- **Your agent can read its own traces.** `--mcp` serves a [Model Context Protocol](docs/guide/mcp.md) endpoint from the same binary — ten tools shaped like questions (what is failing, what is slow, where did the money go), results bounded in tokens, stored span text confined as untrusted, and no fetcher, shell, or outbound path behind the boundary for an injected instruction to actuate.
- **Small enough to trust.** Two direct dependencies; HTTP, threading, and file I/O are the Rust standard library. The in-crate SHA-256 (content addressing only, never authentication) and index hash (not a cryptographic commitment — every probe re-verified against the record) are pinned by test vectors. Every performance number is measured by a bundled benchmark, with anything extrapolated marked as such.
- **Crash-safe by construction, durability you choose.** Write-ahead log with group commit; `--durability` is `buffered`, `wal` (default), or `flushed`, every response says which one answered, and a SIGKILL suite holds each mode to its claim. One caveat stated plainly: macOS `fsync` does not flush the drive's own cache. See [durability](docs/operations/durability.md).

## Performance

Measured on macOS/aarch64 (10 hardware threads) by the bundled benchmarks over the real HTTP path: **208,973 spans/s** sustained ingest at 16 clients in `wal` mode on an idle machine ([ingest.md](docs/benchmarks/ingest.md)); trace lookup **p95 0.64 ms** and filtered search **p95 3.3 ms** on a 1M-span corpus; compaction worth 16–28x on filtered search at 100M spans. **Where Traza is expensive: disk** — uncompressed segments cost 1.8–2.1x the bytes sent, worse than Elasticsearch; the exception is pinned agent context, measured 121:1 in Traza's favour. Full records, caveats, and an honest list of what is *not* measured: [capacity](docs/operations/capacity.md) and [storage comparison](docs/storage-comparison.md). Run the benchmarks on your hardware rather than trusting ours.

## Status

Pre-1.0 and honest about it: on-disk formats may change between 0.x versions, single-node is the current scope, and the known architectural gap (query-visible state spans several recovery domains; the generation/checkpoint boundary that closes it is [designed](docs/generations-design.md) and scheduled before 1.0) is stated rather than implied. The phased roadmap — durable v1 foundations, replicated HA, columnar analytics at billion-span scale — lives in [docs/roadmap.md](docs/roadmap.md).

## Documentation

Organised by what you are doing, in **[docs/](docs/README.md)**:

- **Using** — [getting started](docs/guide/getting-started.md) · [data model](docs/guide/data-model.md) · [ingest](docs/guide/ingest.md) · [HTTP API](docs/guide/http-api.md) · [MCP server](docs/guide/mcp.md) · [trace browser](docs/guide/trace-browser.md) · [LLM semantics](docs/llm-semantics.md)
- **Operating** — [deployment](docs/operations/deployment.md) · [durability](docs/operations/durability.md) · [administration](docs/operations/administration.md) · [monitoring](docs/operations/monitoring.md) · [capacity](docs/operations/capacity.md) · [configuration](docs/configuration.md)
- **Changing** — [architecture](docs/internals/architecture.md) · [invariants](docs/internals/invariants.md) · [testing](docs/internals/testing.md) · [benchmarking](docs/internals/benchmarking.md) · [segment format](docs/segment-format.md) · [CONTRIBUTING.md](CONTRIBUTING.md)

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). The short version: stable Rust is the only dependency, `./ci.sh` is the merge bar, and new dependencies need a reason.

## License

Copyright © 2026 Toshish Jawale. Licensed under the [Apache License, Version 2.0](LICENSE). Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be licensed as above, without any additional terms or conditions.
