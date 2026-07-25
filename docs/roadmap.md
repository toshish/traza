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

- **One binary.** Every capability ships in `traza-server`. No mandatory
  external control-plane database, coordinator, or lock service.
- **Own the data model, speak the standards.** Native conventions stay
  simple and stable; OpenTelemetry (including GenAI conventions) is a
  supported dialect at the boundary, normalized on ingest — never a
  constraint on the engine.
- **Honest numbers, honest status.** Benchmarks are measured by shipped
  code; capabilities are documented only once they exist; formats are
  versioned and migrations are automatic.
- **Identity before features.** Anything that must appear in a key, a
  record header, or an addressing scheme ships *before* the format freeze.
  Features can be added; identity cannot be retrofitted.
- **Small enough to audit.** Dependencies are budgeted, not vibes-based.

## Where Traza stands (baseline, v0.15)

Shipped and load-bearing: durable segment engine (immutable indexed
segments, crash recovery, journaled TTL compaction, larger-than-RAM
file-backed reads), primary-key idempotent ingest, OTLP/HTTP protobuf+JSON,
sessions and token/cost analytics, content-addressed payload offloading,
append-only annotations, streaming NDJSON export with integrity trailers,
bearer auth with scopes, safe bind defaults, a standalone trace-browser UI,
measured benchmarks (116k spans/s ingest; sub-ms trace lookup at 10M spans).

**Known gap that shapes Phase 1:** `POST /v1/spans` acknowledges after the
write buffer accepts the batch, and durability begins at segment flush. A
crash can therefore lose acknowledged writes (bounded by `--flush-spans`).
This is documented in the README and is *not* a production durability
contract. Closing it is the first item below.

Also not yet: replication/HA, query language, columnar analytics at
billion-span scale, tenancy, RBAC/SSO, targeted deletion, encryption at
rest, tail sampling, content search, an eval entity model, packaged
releases.

## What production users expect in 2026 (research summary)

From surveying the current state of the art — general trace backends
(Grafana Tempo, ClickHouse-based stacks, VictoriaTraces, Jaeger v2) and
LLM/agent platforms (Langfuse, LangSmith, Braintrust, Arize Phoenix, W&B
Weave, Opik, AgentOps, Laminar):

- **The market supports the thesis.** ClickHouse
  [acquired Langfuse](https://clickhouse.com/blog/clickhouse-acquires-langfuse-open-source-llm-observability)
  (Jan 2026), pairing the leading open-source LLM observability platform
  with a columnar database vendor. Braintrust built
  [Brainstore](https://www.braintrust.dev/blog/brainstore-architecture), a
  purpose-built AI-log database: a single Rust binary whose segments carry
  a row store, an inverted index, and a column store, with an
  object-storage WAL and a tiered read merge — but requiring Postgres for
  metadata and Redis for locks. Both data points say LLM observability at
  scale is a database problem. Traza's differentiation is being that
  database with no external control plane.
- **Eval-first is the workflow bar, not a feature.** The Braintrust loop —
  datasets of examples, task runs, scorers, experiments diffing score
  distributions, failing production traces promoted into regression
  datasets — is what serious AI teams now mean by observability. A trace
  store that cannot represent experiments, dataset versions, and scores as
  queryable records is substrate for someone else's product. *This roadmap
  therefore treats a minimal eval model as a 1.0 requirement, not a later
  phase.*
- **Columnar + object storage is a proven scale architecture.** Tempo
  writes [Parquet blocks to object storage](https://grafana.com/docs/tempo/latest/reference-tempo-architecture/block-format/);
  ClickHouse stacks query wide events in LSM columnar storage. Row storage
  wins lookup, columnar projections win analytics, object tiering wins cost.
- **Agent models are outgrowing "a list of LLM calls."** The 2026
  differentiators are session/goal-level outcomes, multi-agent step graphs,
  time-travel replay, loop/runaway detection, and evals wired to production
  traces.
- **OTel GenAI conventions are coming but unstable.** `gen_ai.*` semantics
  remain in [Development status](https://opentelemetry.io/docs/specs/semconv/configuration/version-selection/);
  attribute names may still change. The durable strategy is dual-dialect:
  accept and normalize both OTel GenAI and native conventions.
- **Enterprise table stakes are unchanged:** tenancy, RBAC, SSO, audit
  logs, PII controls, retention tiers, sampling, export **and deletion** on
  demand. Their *administration* can ship late; their *identity* cannot.

---

## Phase 1 — Durable, complete v1 foundations (→ v1.0)

*Goal: a team can run one Traza node in production, on purpose, and defend
the choice — and nothing in v1.0 blocks what comes after. 1.0 is a promise,
not a birthday.*

### 1.1 Durability and the acknowledgement contract — **delivered in 0.15**

Ack-before-durability was the single largest gap between Traza and a
production database. It is closed: the write-ahead log, the three explicit
modes, ordered recovery, and log reclamation all ship, with `wal` the
default and a SIGKILL suite holding each mode to its own claim. What remains
of this section is the reporting half — `/metrics` lands with §1.4.

- **Write-ahead log with group commit.** Ingest appends a batched WAL
  record and fsyncs before acknowledging; segment flush stays asynchronous
  and unchanged. Group commit amortizes fsync across concurrent batches so
  durability does not cost the throughput target (per-request synchronous
  segment flush would, and is not the design).
- **Explicit durability modes**, selected per deployment and reported in
  responses and `/metrics`:
  - `buffered` — today's behavior; acknowledged means accepted in memory.
    Remains available for laptop/CI use, and is *documented as lossy*.
  - `wal` (default for production) — acknowledged means fsynced to the WAL
    and recoverable.
  - `flushed` — acknowledged means present in a sealed segment.
- **Recovery** replays the WAL into the write buffer on open; WAL segments
  are reclaimed once the spans they carry are sealed. Payload writes and
  annotation appends join the same ordering discipline.
- **The acknowledgement contract is documented per endpoint**, and the API
  never implies more durability than the configured mode provides.

This work is also the foundation Phase 2 needs: the HA design's replicated
log and applied-index recovery contract assume a durable local state
machine underneath them.

### 1.2 Identity and schema foundations (freeze-critical)

Everything here changes keys, record headers, or addressing. It must exist
in v1 — as reserved, versioned representation even where administration
ships later — or the format freeze is not credible.

- **Tenant identity in the primary key.** Span identity becomes
  `(tenant, trace_id, span_id)`. Without tenant scoping, two tenants
  generating the same trace ID silently upsert over each other — a
  cross-tenant data-loss bug, not a namespacing inconvenience. Single-tenant
  deployments use a default tenant and see no behavioral change. Tenant
  scoping extends to sessions, annotations, payload references, retention
  policy, and quota accounting.
- **Tombstone identity for deletion.** Erasure requires a replicated,
  ordered deletion record that names its subject (tenant, trace, span,
  session, or payload reference) and supersedes prior versions under the
  same last-write-wins discipline as span upserts. Compaction honors
  tombstones; queries never return tombstoned content even before the
  rewrite runs.
  - **Deletion must be reference-aware.** Content-addressed payload dedup
    means one blob may back many spans — potentially across tenants.
    Deleting one tenant's data must not delete a payload another tenant
    still references; the live-reference discipline the TTL sweep already
    uses is the model, extended to per-tenant reference counting.
- **Key-version metadata on encrypted-at-rest artifacts.** Segment,
  payload, WAL, and annotation headers reserve a key-version field so
  encryption and key rotation (Phase 4) do not require a format break.
- **Eval entity model** — a real addressing scheme, not a storage trick:
  - `Dataset` — stable dataset ID, name, tenant.
  - `DatasetVersion` — immutable, content-addressed manifest listing
    example IDs and digests; records its **parent version** for lineage,
    plus provenance (the query or import that produced it).
  - `Example` — **stable example ID** stable across versions, with input,
    optional expected output, split label (train/dev/test), and provenance
    back to the source trace/span it was promoted from.
  - `Experiment` — stable ID linking one dataset version to a set of task
    runs (traces), with configuration metadata.
  - `Score` — addresses the `(experiment, example, span)` tuple. Today's
    `Annotation` addresses only `(trace_id, span_id)` and cannot express
    this, so v1 generalizes annotation addressing to a typed subject
    (trace/span/session/experiment-example) with the existing fields
    preserved.
  - **Deletion and lineage semantics are defined up front:** deleting
    source traces must not corrupt dataset versions that referenced them
    (examples carry their own copies), and deleting a dataset version is
    itself a tombstone with defined effects on dependent experiments.
- **Tail sampling.** Head sampling alone cannot express the rule this
  product exists to serve — "keep every trace whose eval failed" is a
  post-hoc decision. Tail sampling needs a buffered decision window and a
  retention-decision record, both of which touch storage semantics; the
  mechanism ships in v1 even if policies expand later.

### 1.3 Product-thesis minimum

By the roadmap's own definition, a trace store without these is substrate,
not a product. A v1.0 that passed every other gate while failing this test
would be a planning failure.

- **Minimal eval workflow, end to end:** promote failing production traces
  into a dataset version, run an experiment (task execution stays external),
  record scores, and query score distributions and experiment-over-experiment
  diffs as ordinary rollups.
- **Minimal content search:** substring/token matching over span names,
  string attributes, event content, and payload previews, executed through
  the existing index-then-verify path. This validates the debugging
  primitive ("find the session where the model said X") without waiting
  for the full per-segment inverted index in Phase 3.

### 1.4 Operability and release engineering

- **HTTP/1.1 keep-alive + gzip.** Per-request TCP handshakes cap the ingest
  path. (gRPC stays out; `http/protobuf` covers OTel SDKs.)
- **Streaming interactive searches.** `/v1/spans` adopts the chunked cursor
  machinery the export path already proves.
- **OTel GenAI dialect normalization.** Ingest-time mapping of `gen_ai.*`
  (agent spans, tool calls, content events, MCP attributes) onto native
  conventions; dual-dialect queries so either vocabulary finds the data.
- **Prometheus `/metrics`.** Ingest rate, WAL fsync latency, queue depth,
  flush latency, segment counts, query latency histograms, payload store
  size, compaction progress, durability mode.
- **Backup/restore.** Consistent snapshot of a live store: sealed segments,
  WAL position, annotations, payload bytes, eval records, and a checksummed
  manifest; documented restore drill; `traza-server --verify`. This
  generation/checkpoint mechanism becomes the HA snapshot substrate.
- **Ingest-time controls.** Head sampling, field-level PII redaction
  (drop/hash named attributes before storage), per-service retention
  overrides.
- **Release engineering.** GitHub Actions CI (the current `ci.sh` gate),
  prebuilt binaries and container image, crates.io publication, versioned
  docs site.
- **Wire and format stability contract.** Frozen v1 HTTP surface; on-disk
  format versioning with automatic forward migration; documented deprecation
  policy. Post-1.0: no breaking change without a major version and a
  migration path.

### 1.5 Acceptance gates for 1.0

Gates are only credible if reproducible, so the **reference environment is
specified, not implied**: named CPU model and core count, RAM, storage class
(local NVMe, with model and queue depth), filesystem, OS/kernel, durability
mode, payload size distribution, attribute cardinality, client concurrency,
query selectivity, and hot/cold cache state. The benchmark records all of it
alongside results.

- **Durability:** kill -9 property tests in CI prove zero loss of writes
  acknowledged under `wal` and `flushed` modes, and zero torn reads, across
  ingest, annotation, payload, and compaction paths. `buffered` mode is
  tested to lose *only* unacknowledged-as-durable data.
- **Throughput:** 250k spans/s sustained on the reference environment in
  `wal` mode (keep-alive + protobuf), measured by the bundled benchmark.
- **Query latency:** p99 < 10 ms trace lookup and < 50 ms filtered search at
  a 100M-span store; RSS remains O(indexes). *Status at 10M (0.15):* trace
  lookup p99 4.65 ms already clears its bar and RSS held at 0.25 GB, but the
  filtered-search bar is the open risk — uncompacted, it measured p99 220 ms
  at a tenth of the gate's corpus, because the cost is per-segment rather
  than per-span. Size-tiered compaction bounds the segment count; whether
  that is enough at 100M is unproven and needs measuring at that size.
- **Regression policy:** each gate runs ≥5 times; the reported statistic is
  the median with an interquartile range; a release blocks when the median
  regresses >10% *and* the change exceeds run-to-run noise for that metric.
  Single-run comparisons never gate a release.
- **Security posture:** no unsafe code; fuzzed wire surfaces (HTTP framing,
  protobuf decoder, segment parser, WAL reader) in CI; documented threat
  model.
- **Product-thesis gate:** the §1.3 workflow runs end to end in CI against
  the built binary.

## Phase 2 — High availability and agent-native depth (v1.x → v2.0)

*Two tracks in parallel: one makes node failure a non-event, the other makes
Traza the best place to understand agent behavior.*

### 2a. High availability

The Raft direction is chosen; the [HA design](ha-design.md) defines the
v0.13 state inventory and the phase-zero engine, dependency, and protocol
gates that must close before networked HA is exposed.

- **Quorum-replicated logical log** for every query-visible mutation. The
  log — not the leader's volatile write buffer — is the recovery authority;
  success requires quorum durability plus leader visibility.
- **Validated full-state snapshots** covering segments, annotations, payload
  bytes, eval records, retention state, tombstones, and retry outcomes.
- **Explicit read consistency:** linearizable logical reads on the leader;
  follower reads are opt-in, labeled with their applied index. Bounded
  staleness is claimed only after a lag bound is enforced and tested.
- **Replicated retention and deletion:** expiration and tombstones ride the
  log, so replicas never disagree about what exists.
- **Rolling upgrades** with one-minor-version skew tolerance; cluster
  join/leave/replace in one command; cluster status in `/metrics`.

**Acceptance:** the HA design's HA-001…HA-017 evidence table, executed
against real processes — RPO 0 for quorum-acknowledged mutations after one
voter failure, RTO < 10 s for leader failover, measured across repeated
fault runs rather than asserted.

### 2b. Agent-native depth

- **Session outcomes and goals.** First-class terminal status
  (`session.outcome`, goal text, resolution attributes) with outcome-rate
  rollups.
- **Step-graph queries.** Link-aware queries (children-of, caused-by,
  retry-chains) over the span links already stored, plus a dashboard graph
  view for multi-agent sessions.
- **Replay bundles.** One-command export of a session — spans, payloads,
  annotations, link graph — as a self-contained bundle for time-travel
  debugging and regression corpora; one-command re-import.
- **Online eval hooks.** A registration surface invoking an external judge
  (webhook or command) on matching new traces, writing verdicts back as
  scores. Traza orchestrates; it never embeds a model.
- **Annotation queues.** Filtered human-review workflows ("unreviewed failed
  sessions"), reviewer attribution, progress tracking.
- **Anomaly surfacing.** Loop/runaway detection (repeated near-identical
  spans, token-burn spikes) and cost-budget alerts from the rollup layer.
- **Semantic similarity search (opt-in).** Externally computed embeddings
  are indexed in a **side index keyed by `(tenant, trace_id, span_id)`,
  stored as its own append-only index family** — *not* inside span segments,
  because an immutable segment cannot absorb an embedding that arrives after
  it was sealed. The side index supports append, supersession, and
  tombstone-driven removal on the same discipline as spans. Traza never
  computes embeddings.

## Phase 3 — Scale-out and analytics (v2.x)

*Goal: billion-span stores with interactive analytics and bounded cost —
the same binary.*

- **Columnar segment projections (format v3).** Alongside the row store,
  segments carry columnar projections of hot fields (service, name,
  duration, session, model, tokens, cost) so aggregation scans read columns,
  not records — embedded in the self-contained segment.
- **Full per-segment inverted index**, generalizing §1.3's minimal content
  search: span names, string attributes, event content, and payload text
  indexed at offload time so search never re-reads the blob store.
- **Aggregation pushdown.** Rollups execute against projections with late
  materialization; target interactive (<1 s) group-bys over 1B spans.
- **Query language.** A small, stable filter/aggregation DSL for the queries
  URL parameters can't express (grouping, span-relationship predicates),
  shaped so a TraceQL-compatibility layer stays possible.
- **Object-storage tier.** Sealed segments age out to S3-compatible storage
  on policy; queries transparently span local and remote tiers with a local
  index cache. This is an optional data-plane tier the operator already
  runs — it is not a control-plane dependency, and Traza runs fully without
  it.
- **Horizontal partitioning** (after replication, not instead of it): shard
  by `(tenant, trace_id)` hash across replica groups; scatter-gather using
  the same total-order cursor semantics the export path proves.

## Phase 4 — Enterprise control plane (v2.x+)

*Goal: pass procurement without becoming a different product. The identity
these features need was reserved in v1; this phase adds administration.*

- **Tenancy administration.** Per-tenant quotas (ingest rate, storage),
  retention policy, token issuance, and usage accounting on the v1 tenant
  keyspace.
- **AuthN/AuthZ.** OIDC/SAML SSO for the dashboard; API tokens minted
  against identities; RBAC scoped by tenant/service/action (read, write,
  annotate, export, admin).
- **Audit log.** Append-only, queryable record of administrative and
  data-access actions.
- **Erasure workflows.** Operator-facing deletion requests, verification
  reports, and prioritized compaction driving the v1 tombstone mechanism to
  physical removal within a stated SLA.
- **Encryption at rest** for segments, payloads, WAL, and annotations, with
  rotation over the v1 key-version metadata.
- **Compliance packaging.** SOC2-supporting controls documentation, data
  residency guidance, BYOC deployment guides (container, Helm, systemd).

---

## Non-functional commitments (cross-phase)

| Dimension | Commitment |
|---|---|
| **Deployment shape** | One binary, every phase. No mandatory external control-plane database, coordinator, or lock service — ever. Optional data-plane tiers (object storage) are operator-owned and never required. |
| **Compatibility** | SemVer on the wire API and on-disk formats from 1.0; automatic forward migration; one-minor-version cluster skew tolerance from 2.0. |
| **Durability** | Acknowledgement means what the configured durability mode says and no more: `wal` (fsynced, default in production) or `flushed` from 1.0; quorum-durable from 2.0. Verified by kill -9 fault injection every release. |
| **Performance** | Published numbers are measured on the specified reference environment, ≥5 runs, median with IQR. A release blocks on a >10% median regression that also exceeds that metric's run-to-run noise. |
| **Security** | `#![forbid(unsafe_code)]`; fuzzed ingest surfaces; constant-time credential comparison; secure-by-default binds; documented threat model by 1.0. **Public API TLS** stays reverse-proxy territory until keep-alive lands, then is reconsidered on evidence; **cluster peer transport requires mutual TLS 1.3 from the first HA release** — these are separate contracts. |
| **Dependencies** | An audited budget, not a slogan: every dependency (direct *and* transitive) is inventoried per release with license, MSRV, and justification. Additions require a written ADR. The HA phase will add a consensus library, async runtime, and TLS provider; that increase is planned, bounded, and must keep standalone builds dependency-minimal via feature flags. |
| **Observability of Traza itself** | `/metrics`, structured logs, and a health endpoint by 1.0; cluster introspection by 2.0. |
| **Documentation** | Every shipped surface documented in the same release; quickstart verified in CI against the built binary. |

## Explicit non-goals

- **Not a general observability suite.** Traces and their analytics — no
  metrics TSDB, no general log storage. (Full-text search over *trace
  content* is in scope; indexing an application log firehose is not.)
- **No mandatory external control plane.** No metadata database, no lock
  service, no coordinator required beside the binary — the constraint that
  distinguishes Traza from otherwise-similar engines that lean on Postgres
  and Redis. Optional data-plane tiers the operator already owns are a
  different thing and are explicitly allowed.
- **Not an eval model host.** Traza orchestrates eval loops and stores
  verdicts; it never runs or embeds judge models.
- **Not a SQL engine.** The DSL stays small and purpose-built.
- **Not a framework SDK.** Instrumentation belongs to OTel and native HTTP.

## Sequencing at a glance

```
v0.14+   1.1 durability (WAL + ack contract) — first, everything rests on it
         1.2 identity foundations (tenant key, tombstones, key versions,
             eval entity model, tail sampling) — before any format freeze
         1.3 product-thesis minimum (eval workflow + content search)
         1.4 operability (keep-alive, metrics, backup, packaging)
v1.0     Stability contract + §1.5 gates met on the specified reference
         environment
v1.x     Phase 2 in parallel: HA (2a) and agent-native depth (2b)
v2.0     Replicated clusters GA; rolling upgrades
v2.x     Phase 3 (columnar, inverted index, object tier, sharding), then
         Phase 4 (enterprise control plane) as demand dictates
```

The ordering principle: **durability, then identity, then the product
thesis, then availability, then scale.** Durability first because every
later guarantee inherits it. Identity second because keys and addressing
cannot be retrofitted after a format freeze. The product thesis third
because a datastore that stores agent traces without closing the eval loop
is, by this document's own definition, someone else's substrate.
