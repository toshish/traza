# Roadmap program: v0.4 → 1.0

One leg at a time, in order. A leg is DONE when: its acceptance oracles pass
under `./ci.sh`, the delivery is integrated (call-site wiring verified, not
module presence), an independent review has re-executed the checks, and the
benchmark gates still pass. Each leg gets its own spec in docs/ before work
begins.

| # | Leg | Scope in one paragraph | Exit gates |
|---|---|---|---|
| 1 | Larger-than-RAM reads | Segments stop holding payload bytes resident: indexes stay in memory, record payloads are read on demand from the file (seek/read; std-only — no mmap dependency). Query results stream from disk. | RSS after open is O(indexes); all 32 tests green; 1M/10M bench gates hold within 2x of current latencies |
| 2 | OTLP/HTTP JSON ingest | `POST /v1/traces` accepting OpenTelemetry OTLP/HTTP JSON (ExportTraceServiceRequest), mapped onto the span model with attributes flattened per OTel semantics. No protobuf, no new dependencies. | conformance tests from recorded OTLP samples; existing wire contract untouched |
| 3 | LLM-observability semantics | First-class conventions for gen-AI spans: prompt/completion payloads, token usage, model name, tool calls — documented attribute schema + indexed keys + query recipes. | schema doc + tests exercising the conventions end to end |
| 4 | Auth | Bearer-token authentication with per-token read/write scopes; constant-time comparison; tokens from config file or env. TLS stays out of scope (reverse proxy). | authz matrix tests; unauthenticated requests rejected; zero deps |
| 5 | Dashboard | A bundled, dependency-free HTML/JS trace browser served by traza-server: trace list, trace waterfall, span detail, filter search over the existing API. | serves from the binary; manual smoke + endpoint tests |
| 6 | HA / replication | Scope to be cut LAST and re-negotiated: likely segment-shipping follower replication with read-only replicas before any consensus work. | separate design doc first; not started until legs 1-5 close |

Program rules (learned the hard way): specs live in this repo before the
leg starts; contracts mirror the spec's gates with executable oracles;
integration is verified at call sites; benchmarks are regenerated, never
edited; a leg that stalls gets concluded honestly and its successor seeded
from the workspace, not restarted from zero.
