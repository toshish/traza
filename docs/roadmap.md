# Traza product roadmap

*Last revised: July 2026. This is the product roadmap — where Traza is going
and in what order. Current shipped state is the baseline below; the
[CHANGELOG](../CHANGELOG.md) is the record of what actually landed.*

## Vision

**The trace database for the agent era: one binary from laptop to cluster.**

Traza's bet is that trace storage should scale like a database, not like a
pipeline — and that LLM/agent telemetry is a first-class workload, not an
attribute convention bolted onto a request tracer. The product wins when:

1. A developer gets from `cargo build` to debugging their agent in under a
   minute, with zero infrastructure.
2. The same binary, with more nodes, carries production traffic with
   replication, failover, and predictable cost.
3. Teams close the loop *inside* the datastore: trace → session → cost →
   eval → dataset, without exporting to three other systems.

### Product principles (these outrank any feature)

- **One binary.** Every capability ships in `traza-server`. No sidecar
  constellation, no mandatory external database, queue, or coordinator.
- **Own the data model, speak the standards.** Native conventions stay
  simple and stable; OpenTelemetry (including GenAI conventions) is a
  supported dialect at the boundary, normalized on ingest — never a
  constraint on the engine.
- **Honest numbers, honest status.** Benchmarks are measured by shipped
  code; capabilities are documented only once they exist; formats are
  versioned and migrations are automatic.
- **Small enough to audit.** Dependencies require a reason. The standard
  library is the default answer.

## Where Traza stands (baseline, v0.13)

Shipped and load-bearing: durable segment engine (immutable indexed
segments, crash recovery, journaled TTL compaction, larger-than-RAM
file-backed reads), primary-key idempotent ingest, OTLP/HTTP protobuf+JSON,
sessions and token/cost analytics, content-addressed payload offloading,
append-only annotations, streaming NDJSON export with integrity trailers,
bearer auth with scopes, safe bind defaults, bundled dashboard, measured
benchmarks (116k spans/s ingest; sub-ms trace lookup at 10M spans).

Not yet: replication/HA, query language, columnar analytics at billion-span
scale, multi-tenancy, RBAC/SSO, PII controls, sampling, targeted deletion,
metrics endpoint, keep-alive HTTP, packaged releases.

## What production users expect in 2026 (research summary)

From surveying the current state of the art — general trace backends
(Grafana Tempo's object-storage Parquet architecture, ClickHouse-based
stacks, VictoriaTraces, Jaeger v2) and LLM/agent platforms (Langfuse,
LangSmith, Braintrust, Arize Phoenix, W&B Weave, Opik, AgentOps, Laminar):

- **The market validated the thesis.** Langfuse — the leading open-source
  LLM observability platform — was acquired by ClickHouse (Jan 2026)
  precisely because LLM observability at scale is a *database* problem.
  Traza's angle is being that database natively, without the two-system
  stack.
- **Columnar + object storage is the scale architecture.** Tempo writes
  Parquet to S3; ClickHouse stacks query wide events in LSM columnar
  storage. Full-fidelity row storage wins for lookup; columnar projections
  win for analytics; tiering to object storage wins for cost.
- **Agent models are outgrowing "a list of LLM calls."** The 2026
  differentiators are session/goal-level outcomes, multi-agent step graphs,
  time-travel replay of a session, loop/runaway detection, and evals wired
  to production traces (LLM-as-judge, human annotation queues,
  auto-generated datasets from failures).
- **OTel GenAI conventions are coming but unstable.** `gen_ai.*` semantics
  (agent spans, MCP tool calls, content events) are still in Development
  status; attribute names may change. The durable strategy is dual-dialect:
  accept and normalize both OTel GenAI and native conventions.
- **Enterprise table stakes are unchanged but non-negotiable:**
  multi-tenancy, RBAC scoped by team/service, SSO (OIDC/SAML), audit logs,
  PII redaction at ingest, retention tiers, sampling/cost controls, data
  export **and deletion** on demand, SOC2/GDPR-supporting controls.

The roadmap below sequences Traza from today's single node to that
destination without breaking the product principles.

---

## Phase 1 — Production-ready single node (→ v1.0)

*Goal: a team can run one Traza node in production, on purpose, and defend
the choice. 1.0 is a promise, not a birthday.*

**Functional**

- **Wire and format stability contract.** Frozen v1 HTTP API surface;
  on-disk format versioning with automatic forward migration; documented
  deprecation policy. Post-1.0: no breaking change without a major version
  and a migration path.
- **HTTP/1.1 keep-alive + gzip** (request and response). Exporters batch
  aggressively; per-request TCP handshakes are the current bottleneck's
  ceiling. (gRPC remains out; `http/protobuf` covers OTel SDKs.)
- **Streaming interactive searches.** Exports already stream with
  integrity trailers; `/v1/spans` still materializes one bounded JSON
  response. Interactive queries adopt the same chunked cursor machinery.
- **OTel GenAI dialect normalization.** Ingest-time mapping of `gen_ai.*`
  (agent spans, tool calls, content events) onto Traza's native
  conventions, tracked against the evolving spec; dual-dialect queries so
  either vocabulary finds the data. MCP tool-call attributes included.
- **Prometheus `/metrics`.** The datastore must be observable itself:
  ingest rate, queue depth, flush latency, segment counts, query latency
  histograms, payload store size, compaction progress.
- **Backup/restore.** Consistent snapshot of a live store (segments are
  immutable — snapshot = hardlink/copy manifest + sealed buffer flush);
  documented restore drill; `traza-server --verify` integrity check.
- **Ingest-time controls.** Head sampling (per-service rate), field-level
  PII redaction hooks (drop/hash named attributes before storage), and
  per-service retention overrides on top of the global TTL.
- **Release engineering.** GitHub Actions CI (the current `ci.sh` gate),
  prebuilt binaries + container image, crates.io publication, versioned
  docs site.

**Non-functional acceptance for 1.0**

- 250k spans/s sustained ingest on reference hardware (keep-alive +
  protobuf path), measured by the bundled benchmark.
- p99 < 10 ms trace lookup and < 50 ms filtered search at a 100M-span
  store; RSS remains O(indexes).
- Crash-safety property test in CI (kill -9 loops against a writing store,
  zero acknowledged-write loss, zero torn reads).
- Zero-warning security posture: no unsafe code, fuzzed wire surfaces
  (HTTP framing, protobuf decoder, segment parser) in CI.

## Phase 2 — High availability (v1.x → v2.0)

*Goal: node failure is an operational non-event. The design is done
([ha-design.md](ha-design.md)); this phase executes it.*

- **Quorum-replicated logical log** for ingest (leader-ordered, per the
  design: the leader's write buffer is the acknowledgment boundary), with
  automatic leader election and client-transparent failover.
- **Segment shipping** for replica catch-up and re-seeding; replicas serve
  reads (read-your-writes on the leader, bounded staleness elsewhere —
  documented, queryable consistency mode).
- **Replicated retention:** expiration decisions ride the log, so replicas
  never disagree about what exists.
- **Rolling upgrades** across a replica set with version-skew tolerance of
  one minor version.
- **Cluster operations:** join/leave/replace a node with one command;
  cluster status in `/metrics` and the dashboard.

**Non-functional acceptance:** automated fault-injection suite (leader
kill, network partition, disk-full, slow follower) with defined recovery
objectives — RPO 0 for quorum-acknowledged writes, RTO < 10 s for leader
failover — measured in CI, not asserted.

## Phase 3 — Scale-out and analytics (v2.x)

*Goal: billion-span stores with interactive analytics and bounded cost —
the same binary.*

- **Columnar segment projections (format v3).** Alongside the row-oriented
  record store, segments carry columnar projections of hot fields
  (service, name, duration, session, model, tokens, cost) so aggregation
  scans read columns, not records. Parquet-inspired, but embedded in the
  self-contained segment — no external file zoo.
- **Aggregation pushdown.** Rollup queries (`/v1/stats/llm`, sessions)
  execute against projections with late materialization; target:
  interactive (<1 s) group-bys over 1B spans.
- **Query language.** A small, stable filter/aggregation DSL for the API
  and dashboard (shaped so a TraceQL-compatibility layer is possible
  later). URL parameters remain the simple path; the DSL is for the
  queries URLs can't express (grouping, span-relationship predicates).
- **Object-storage tier.** Sealed segments age out to S3-compatible
  storage on policy; queries transparently span local and remote tiers
  with a local index cache. This is the cost story at fleet scale — and it
  doubles as the HA catch-up and backup substrate.
- **Horizontal partitioning** (after replication, not instead of it):
  shard by trace-id hash across replica groups; scatter-gather with the
  same total-order cursor semantics the export path already uses.

## Phase 4 — Agent-native depth (parallel track from v1.x)

*Goal: the best place to understand — not just store — agent behavior.
These build on the session/annotation/payload foundations already shipped.*

- **Session outcomes and goals.** First-class terminal status for a
  session (`session.outcome`, goal text, resolution attributes) with
  outcome-rate rollups; sessions stop being just activity windows.
- **Step-graph queries.** Span links already model fan-out/fan-in and
  retries; add link-aware queries (children-of, caused-by, retry-chains)
  and a dashboard graph view for multi-agent sessions.
- **Replay bundles.** One-command export of a session — spans, payloads,
  annotations, link graph — as a self-contained bundle for time-travel
  debugging and regression corpora; one-command re-import.
- **Online eval hooks.** A registration surface that invokes an external
  judge (webhook or command) on matching new traces and writes the verdict
  back as annotations — Traza orchestrates the loop but never embeds a
  model.
- **Annotation queues.** Human review workflows: filtered work queues
  ("unreviewed failed sessions"), reviewer attribution, progress tracking —
  on the existing annotation record.
- **Anomaly surfacing.** Loop/runaway detection (repeated near-identical
  spans, token-burn spikes) and cost-budget alerts computed from the
  rollup layer, exposed as queryable events and dashboard badges.
- **Dataset curation.** Saved export definitions with dedup/split
  conventions, so "the eval set" is a named, reproducible query rather
  than a script someone lost.

## Phase 5 — Enterprise operation (v2.x+)

*Goal: pass procurement without becoming a different product.*

- **Multi-tenancy.** Isolated tenants in one deployment: per-tenant
  keyspace, quotas (ingest rate, storage), retention, and tokens. Tenant =
  namespace, not a separate process.
- **AuthN/AuthZ.** OIDC/SAML SSO for the dashboard and API tokens minted
  against identities; RBAC with scopes by tenant/service/action (read,
  write, annotate, export, admin).
- **Audit log.** Append-only, queryable record of administrative and
  data-access actions.
- **Targeted deletion (GDPR/erasure).** Tombstone records + prioritized
  compaction rewrite deliver verifiable deletion from immutable segments,
  payloads, and annotations — the design must respect the primary-key and
  supersede semantics that every satellite layer already honors.
- **Encryption at rest** (segment + payload files) with key rotation;
  TLS remains the reverse-proxy's job until keep-alive lands, then native
  TLS is reconsidered on evidence.
- **Compliance packaging.** SOC2-supporting controls documentation, data
  residency guidance, BYOC deployment guides (container, Helm, systemd).

---

## Non-functional commitments (cross-phase)

| Dimension | Commitment |
|---|---|
| **Deployment shape** | One binary, every phase. A feature that requires a second mandatory process is out of scope by definition. |
| **Compatibility** | SemVer on the wire API and on-disk formats from 1.0; automatic forward migration; one-minor-version cluster skew tolerance from 2.0. |
| **Durability** | Acknowledged means recoverable: quorum-acknowledged from Phase 2. Crash-safety verified by fault-injection CI, every release. |
| **Performance** | Every release re-runs the bundled benchmark on reference hardware; regressions >10% block the release. Published numbers are always measured. |
| **Security** | `#![forbid(unsafe_code)]`; fuzzed ingest surfaces; constant-time credential comparison; secure-by-default binds; documented threat model by 1.0. |
| **Observability of Traza itself** | `/metrics`, structured logs, and a health endpoint by 1.0; cluster introspection by 2.0. |
| **Dependencies** | Each new dependency needs a written justification in the PR; the count stays countable on one hand. |
| **Documentation** | Every shipped surface documented in the same release; quickstart verified in CI against the built binary. |

## Explicit non-goals

- **Not a general observability suite.** Traces and their analytics —
  no metrics TSDB, no log search engine. Correlate by trace-id with
  whatever owns those.
- **Not an eval model host.** Traza orchestrates eval loops and stores
  verdicts; it never runs or embeds judge models.
- **Not a SQL engine.** The DSL stays small and purpose-built; teams who
  need SQL can export or read segments directly (format documented).
- **Not a framework SDK.** Instrumentation belongs to OTel and native
  HTTP; Traza competes on the datastore, not on client libraries.

## Sequencing at a glance

```
v0.14+   Phase 1 items land incrementally (keep-alive, gzip, metrics,
         GenAI dialect, sampling/redaction, backup, release engineering)
v1.0     Stability contract + Phase 1 acceptance gates met
v1.x     Phase 2 (HA) + Phase 4 (agent depth) in parallel
v2.0     Replicated clusters GA; rolling upgrades
v2.x     Phase 3 (columnar, object tier, partitioning) + Phase 5
         (enterprise) as demand dictates
```

The ordering principle: durability and honesty first (done), production
single-node credibility second, availability third, scale and enterprise
surface last — because each layer is only worth building on a trustworthy
version of the previous one.
