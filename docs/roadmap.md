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

## Where Traza stands (baseline, v0.16)

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
- **Minimal content search** — **shipped**, and larger than "minimal". Word
  matching over string attributes, nested message arrays, event attributes and
  event names, via `?content=`, executed through the existing
  index-then-verify path. It did not wait for a per-segment inverted index
  because it did not need one: a per-block Bloom filter over words, stored
  bit-sliced so a probe reads tens of bytes per segment, measured **849-1,554x
  against a scan** on selective terms at 200,000 spans, for +0.1% on disk and
  ~2 KiB resident per segment.

  Scope actually delivered is narrower than the line above in one way and
  wider in another. It is **word** matching, not substring matching — a word
  index cannot soundly drive a substring query, and the alternative was
  silently wrong answers rather than slow ones. Payload previews are covered
  only where the reference object is reachable as a string attribute. See
  [the segment format](segment-format.md#the-content-index).

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

### 1.5 One recovery domain: generations and checkpoints

Traza currently has several independent recovery domains — the write-ahead log
and write buffer, segments, `annotations.jsonl`, `payloads/`, retention
decisions, and export pagination. Each has its own durability rule and its own
idea of "now"; nothing names a state they all agree on.

An independent review of v0.17 found four defects that are, read together, four
symptoms of that: retention changed memory but not the recovery authority;
export paged a store that moved underneath it; log recovery could not tell safe
tail damage from damage that changes what the store contains; and the flush
policy bounded a recovery cost with a memory-shaped number. All four are fixed
and tested, but each fix leaves behind a rule a future change has to remember
rather than a boundary it cannot cross.

The fix for the class is the generation/checkpoint model already specified for
HA — an immutable, self-describing, complete logical state published by one
atomic `CURRENT` rename, with `pin`, `checkpoint`, `verify` and `install` as
its operations. It belongs **here**, in Phase 1: it changes the directory
layout, so it must land before the format freeze, and once it exists backup,
export, retention and replication become versions of one operation instead of
four independent ones. Phase 2 then adds consensus on top of an engine that
already has the boundary Raft assumes, rather than introducing both at once.

Design and sequencing: [generations and checkpoints](generations-design.md).

**Acceptance:** an existing data directory migrates to generation zero at first
open; backup is pin-verify-copy with no server stop; export pins spans,
annotations and payloads together; a published deletion is durable when
`CURRENT` moves and provably absent from every domain afterwards.

### 1.6 Acceptance gates for 1.0

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
  `wal` mode, measured by the bundled benchmark. ***Status (0.16): 250,453
  spans/s with `--profile throughput` — the median clears the bar, but NOT
  YET CONFIRMED***: median of 5 rotated rounds at concurrency 16 over a
  1M-span corpus, min 122,768, max 261,215 (`ingest-bench`; see
  [INGEST-BENCHMARK.md](../INGEST-BENCHMARK.md)). The spread straddles the
  target because the host was shared during measurement (1-minute load average
  6.5 to 47.8, mean 15.4). **Re-measure on an idle machine before calling this
  met.** At the default `balanced` settings the same run measures 197,056, and
  108,881 was the figure before persistent connections and the decode/WAL work
  landed.

  The parenthetical this line used to carry — "(keep-alive + protobuf)" — did
  not survive measurement, but not for the reason first recorded here. This
  line briefly claimed protobuf was *slower* than JSON; that came from a
  benchmark that posted JSON to `/v1/spans` and protobuf to `/v1/traces`, so
  the difference contained the whole OTLP mapping and could not isolate the
  wire format. Measured properly, with both encodings on `/v1/traces` and both
  decoders lowering straight to `Span`, **protobuf decodes 2.3–2.7x FASTER
  than OTLP JSON** (479 vs 1,275 ns/span at concurrency 1) on payloads 2.9x
  smaller. It is the cheaper wire format. It still does not move this gate,
  because **decode of any kind is 1.9% of ingest cost** — that, not a slow
  protobuf, is why the attribution fails. Keep-alive is worth +11% at
  batch=20 and nothing at batch=1000.

  The limit was the **writer lock, of which segment sealing was most of the
  time held** — work that needs no lock. An earlier version of this line
  called that "a hard ceiling near 212k spans/s on the current design
  regardless of client concurrency". **That was wrong.** The arithmetic behind
  it was correct but was performed at a single value of `--flush-spans`
  (10,000, the default). A seal carries a fixed cost — two fsyncs, a create
  and rename, a reopen-and-parse — on top of its per-span cost, so sealing
  less often amortizes the fixed part and the ceiling moves. It is a function
  of the setting, not a property of the design.

  **Sealing now runs with no engine lock held (0.19).** Re-measured on
  `ddd185a` before the change, at concurrency 8 on a 1M-span corpus, in-lock
  work (wal write + buffer upsert + segment seal) was **4,911 ms at
  `--flush-spans 10000`** — lock held 88%, of which sealing was 74% — and
  **6,371 ms at `--flush-spans 5000`**, lock held 97%. PR #15's WAL rework did
  not change that conclusion; it only made `wal fsync` the next candidate. The
  seal now drains the buffer under a short lock, writes with nothing held, and
  publishes under a short lock, exactly as compaction and expiry do.

  Measured before and after with the two builds alternated round-robin on a
  contended host, median of four rounds at concurrency 8: `throughput`
  162,763 → **222,683** (+37%), `balanced` 116,612 → **176,004** (+51%),
  `latency` 83,400 → **180,331** (+116%). Those levels are depressed by the
  load; the ratios are what the round-robin supports. The structural result is
  that **`--flush-spans` stopped being a throughput knob** — `latency` and
  `balanced` now measure within 3% of each other, where before they spanned
  2x.

  Two v0.17 conclusions recorded here were wrong and are corrected in
  INGEST-BENCHMARK.md: moving the seal off the lock needs **neither** a
  reader-visible "sealing" tier (the spans simply stay in the write buffer
  until the segment is published, as a merge keeps its inputs live) **nor** a
  rotating WAL (reclamation rides `--flush-wal-bytes` instead of running on
  every seal).

  **The gate is still open and the 0.19 matrix cannot close it.** It was taken
  with an unrelated process holding a core throughout, and it puts concurrency
  16 *below* concurrency 8 at every profile — oversubscription, not an engine
  limit. What changed is the *reason* the gate is open: sealing is no longer
  the constraint (10-15% of in-lock work, from 74-81%), and `wal write` now is,
  at 78-85%. **Re-measure on an idle machine**, and expect the log device
  rather than the engine's locking to be what the answer turns on.
- **Query latency:** p99 < 10 ms trace lookup and < 50 ms filtered search at
  a 100M-span store; RSS remains O(indexes). **The RSS half of this gate is
  mis-stated and is not currently met for LLM corpora** — see the memory gate
  below; it is kept here only because the latency halves are measured against
  it. *Status at 10M (0.15):* trace
  lookup p99 4.65 ms already clears its bar and RSS held at 0.25 GB, but the
  filtered-search bar is the open risk — uncompacted, it measured p99 220 ms
  at a tenth of the gate's corpus, because the cost is per-segment rather
  than per-span. Size-tiered compaction bounds the segment count, and the
  gate has now been measured at its real corpus size, against a measured
  uncompacted baseline rather than an extrapolation. **At 100M spans
  (~55 GB) with `--compaction-max-segment-bytes 1073741824` (1 GiB): trace
  lookup p99 0.99 ms and filtered search p99 22.2 ms both PASS the gate**
  (p50 2.3 ms, p95 9.3 ms). With the 256 MiB default the same corpus
  measures filtered-search p99 72.9 ms and MISSES the bar, so the binding
  constraint was the cap, not the algorithm — it floors segment count near
  corpus/cap, and the measured count fell from ~380 to ~100-125. Uncompacted
  the same query measures p99 1664.6 ms.
  **Both latency criteria are met at 100M and only at 100M.** They are
  untested above that size, on one machine, in a single run — the regression
  policy below (≥5 runs, median with IQR) has not yet been applied to them.
  The RSS criterion is the one this trade puts under strain: peak RSS is
  6.7 GB at the 1 GiB cap against 2.0 GB at the default, because a merge
  materializes its inputs, so the peak tracks the cap rather than the index
  size. Between merges resident memory settles around 1.4-2.4 GB, so the
  transient is plainly not O(indexes); the steady state is, but only in the
  sense the memory gate below now spells out, and every figure in this
  paragraph was measured on a corpus with five enum-valued attributes.
  Sustained ingest also falls from 40,894/s to
  31,267/s. Segment count still grows with the corpus, so the tail returns
  at a large enough store, and the Phase 3 per-segment inverted index
  remains the structural answer.
- **Memory:** *this gate replaces the "RSS remains O(indexes)" clause above,
  which was true only for the corpus it was measured on.* Resident memory is
  bounded independently of corpus size for **low-cardinality** indexed
  attributes and is **linear in indexed text** otherwise, because the
  attribute index is keyed on the whole attribute value and
  `Segment::open` decodes it eagerly. Measured (`index-mem-bench`): 10M spans
  with six enum-valued attributes open in 846 MiB, entirely postings at
  8 bytes per span per indexed attribute; one 2 KiB indexed prompt per span
  measures RSS ≈ 1.44 × the text ingested, extrapolating to ~29 GB at 10M
  spans. Deduplication is per segment, so global repetition does not help
  once distinct values exceed `--flush-spans` — 10% and 100% unique cost
  identically at 512 B and 2 KiB values. Full results:
  [capacity](operations/capacity.md#memory).
  **The bar for 1.0:** an LLM corpus of 10M spans carrying a 2 KiB indexed
  prompt each serves within 4 GB RSS. That needs the Phase 3 hashed-key or
  dictionary-encoded attribute index (store a digest, not the value —
  `payload::sha256_hex` truncated, never `DefaultHasher`, which is randomly
  seeded per process and cannot be persisted). Until then the documented
  mitigation is `--payload-threshold-bytes`, and the docs say so rather than
  implying the problem does not exist.
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
- **Full per-segment inverted index**, generalizing §1.3's shipped content
  search. The Bloom-filter index that shipped answers "which blocks may hold
  this word" and nothing more, which is why the three things it cannot do all
  come from the same root — it stores no postings:

  - **Substring and prefix matching.** `refund` cannot find `refunds`.
  - **Phrase and proximity.** A multi-word query is a conjunction; word
    ORDER is not recorded.
  - **Ranking.** There are no term frequencies, so there is nothing to rank
    by; results come back in Traza's stable span order.

  Postings would also let a common term stop costing a scan — today a word in
  nearly every span measures 1.0x against no index at all, which is honest but
  is the case the current design cannot improve. Payload text should be
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
v1.0     Stability contract + §1.6 gates met on the specified reference
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
