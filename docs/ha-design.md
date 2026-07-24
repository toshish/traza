# Traza high-availability design

## Status and baseline

This is a pre-implementation architecture, not shipped behavior. It is grounded in the
v0.13.0 engine and public API (`ac69f44`) and supersedes the earlier v0.9-oriented
proposal. It defines the safety contract and the engine work required before HA may be
exposed. It does not claim that replication, failover, cluster security, or operational
readiness exists today.

The protocol direction is decided:

- one Raft leader accepts mutations;
- a successful mutation is durable in a voting quorum and visible on the leader;
- the Raft log is the recovery authority for committed logical state;
- every node materializes that state through its own Traza engine directory;
- follower reads are stale and opt-in; logical reads are linearizable by default;
- physical segment creation and compaction remain local implementation details;
- three voters are the minimum supported production topology.

The design is not implementation-ready until the phase-zero gates in
[Implementation sequence](#implementation-sequence) close the engine checkpoint,
command-application, consensus-library, TLS, and dependency decisions.

## Goals and non-goals

HA must preserve these properties:

1. No acknowledged mutation is lost after any single voter fails.
2. A minority partition never acknowledges a mutation.
3. Every promoted leader contains the committed logical history.
4. A default logical read observes one state at one committed index.
5. Restart, replay, and snapshot installation cannot duplicate non-idempotent effects.
6. Spans, annotations, payload bytes, retention state, and retry outcomes fail over
   together.
7. Standalone mode retains the v0.13 API and storage behavior.

This design does not provide multi-leader writes, Byzantine-fault tolerance,
cross-cluster federation, geographic placement, sharding, or automatic recovery after
the permanent loss of a voting majority. Those require separate designs.

## Current v0.13 state and HA authority

Traza does not have one monolithic data file. HA must cover every query-visible state
surface, not only `.seg` files.

| State or operation | Current v0.13 persistence | HA authority and command |
|---|---|---|
| Span batch from `/v1/spans` or OTLP | Primary-key upsert into `WriteBuffer`, then immutable v2 segments | Replicated `SpanBatch*` transaction; `(trace_id, span_id)` remains the logical key |
| Large span/event text | Content-addressed files under `payloads/`, referenced from spans | Replicated `PayloadChunk` and `PayloadSeal` before a referencing span batch commits |
| Annotation append | Fsynced `annotations.jsonl` plus an in-memory trace index | Replicated `AnnotationAppend` carrying the leader-resolved timestamp |
| TTL expiration | `Store::compact_expired` rewrites spans and annotations and sweeps payload files | Replicated staged `RetentionPlan*` transaction; filesystem mtime is not an HA decision source |
| Explicit flush | `POST /v1/flush` seals the local write buffer | Replicated `FlushBarrier`; leader response waits for local seal through the barrier |
| Request deduplication | No general request-ID table | Replicated bounded outcome table keyed by authenticated principal and `Idempotency-Key` |
| Segment indexes and analytics rollups | Derived from immutable segments | Rebuilt locally; never independent consensus authority |
| Segment supersede journal | Local crash recovery for physical replacement | Local only; it must not consume a Raft index or decide logical visibility |
| `/v1/stats` physical counters | Local buffer, segment count, and bytes | Node-local diagnostic state, explicitly not a linearizable cluster total |
| Term, vote, membership, log and snapshot metadata | Not present | Consensus storage under the node-private `raft/` directory |

`src/lib.rs` is the application-state-machine boundary. `src/segment_v2.rs` remains
the segment encoder and validator. `src/annotations.rs` and `src/payload.rs` are part of
the state machine, not optional sidecars. `src/expiration.rs` is currently only a module
placeholder; retention behavior lives in `Store::compact_expired` and
`Store::expire_before`. The scheduler in `src/bin/traza-server.rs` must run only on the
leader in HA mode.

The complete existing regression suite remains mandatory, including auth, dashboard,
ingest hardening, LLM analytics, OTLP JSON and protobuf, payload/annotation/export,
segment-format, server-on-engine, and storage tests. A benchmark is not a correctness
oracle.

## Node layout and ownership

Each replica has a distinct data directory and acquires its own local `DirectoryLock`.
That lock prevents two local `Store` owners; it does not establish cluster leadership.
The proposed layout separates consensus state from replaceable engine generations:

```text
data/
  LOCK
  raft/
    hard-state
    log/
    snapshots/
  generations/
    <generation-id>/
      engine/
        segment-*.seg
        annotations.jsonl
        payloads/
      state-manifest.json
  incoming/
  CURRENT
```

`CURRENT` names one complete generation. Consensus metadata is never inside a directory
that snapshot installation swaps. A generation remains immutable after replacement,
except for deletion once no reader or export pins it.

Every node has a stable operator-provisioned node ID, cluster ID, peer address, client
address, and data directory. Node identity is not inferred from an ephemeral address.
A node persists its cluster ID and rejects log, snapshot, or membership traffic for any
other cluster. Reusing a removed identity or directory requires an explicit recovery
procedure.

## Consensus protocol and implementation boundary

Traza uses Raft semantics, not an ad hoc heartbeat lease:

- persisted term and one vote per term;
- log matching and leader completeness;
- commitment by a voting majority, including the current-term commit restriction;
- pre-vote and quorum checking to reduce disruptive elections;
- ReadIndex-style quorum confirmation for linearizable reads;
- learners for catch-up and joint consensus for voter changes;
- no clock-based leader lease in the first implementation.

The consensus implementation must be a reviewed library rather than new handwritten
Raft. The current integration candidate is
[OpenRaft 0.9.x](https://docs.rs/openraft/latest/openraft/) because it exposes separate
log, state-machine, snapshot, network, membership, and linearizable-read interfaces.
Its pre-1.0 API is explicitly unstable, so this document does not approve a crate
version. Phase zero must pin and evaluate one patch release, record its MSRV and
transitive dependencies, exercise its storage contract under fault injection, and
either approve it in an ADR or reject it before production code depends on it.

The consensus task uses an async runtime isolated behind `src/ha/`. The synchronous
`Store` is reached through one bounded apply worker. Engine locks are never held across
network or consensus awaits, and state-machine application never calls back into
proposal code.

Peer transport uses TLS 1.3 mutual authentication through a reviewed TLS library such
as [rustls](https://docs.rs/rustls/latest/rustls/). Certificates bind cluster ID and
node ID to current membership. A removed node is rejected even while an old certificate
remains cryptographically valid.
Plaintext peer transport is permitted only on loopback in tests. The exact consensus,
runtime, TLS, and async-adapter dependencies require a written dependency and MSRV ADR;
standalone builds must remain available without enabling HA.

## Replicated command protocol

### Envelope

Every application entry is interpreted with:

- command schema version;
- Raft log ID (term and index) supplied by the consensus layer, not embedded in
  the canonical command digest;
- command kind;
- cluster ID;
- canonical payload length and digest;
- optional idempotency identity and canonical request digest;
- deterministic command data.

Entries never depend on follower clocks, random generation, hash-map iteration order,
local paths, or local segment numbers. The leader resolves timestamps and generated
identifiers before proposing the command.

Application entries are bounded to 2 MiB. Client requests may remain as large as the
standalone 64 MiB limit, so large logical operations use staged commands:

- `PayloadChunk { hash, offset, total_length, bytes }`
- `PayloadSeal { hash, total_length }`
- `SpanBatchBegin { batch_id, count, digest }`
- `SpanBatchChunk { batch_id, offset, canonical_spans }`
- `SpanBatchCommit { batch_id }`
- `SpanBatchAbort { batch_id }`
- `AnnotationAppend { annotation, resolved_timestamp_ns }`
- `RetentionPlanBegin { plan_id, cutoff_ns, digest }`
- `RetentionPlanChunk { plan_id, expired_payload_hashes }`
- `RetentionPlanCommit { plan_id }`
- `FlushBarrier`

Payload chunks are content-addressed, idempotent by hash and offset, and invisible until
`PayloadSeal` verifies length and content digest. A `SpanBatchCommit` is proposed only
after all referenced payloads are sealed. Batch chunks remain invisible in staging
until commit, preserving the existing all-or-nothing validation behavior. A crashed or
abandoned preparation is removed only by a replicated abort or a later committed
garbage-collection plan; local age is never sufficient.

Multi-entry transactions are contiguous in the application log. The proposal adapter
does not interleave another logical mutation between a transaction's begin and terminal
commit or abort entry. A new leader reconstructs committed staging state and either
finishes a fully validated transaction or appends an abort; it never infers commit from
the presence of chunks.

The leader builds a retention plan from one applied prefix. The plan contains its cutoff
and the exact payload hashes that become unreachable after spans and annotations expire;
prepared but not yet committed payloads are excluded. `RetentionPlanCommit` atomically
makes the staged plan visible. Every replica therefore removes the same logical spans,
annotations, and payloads. HA retention never uses local payload mtime as an authority.
Physical deletion can be deferred, but a node must not serve content deleted by a
committed plan.

### Request identity

`(trace_id, span_id)` is a record key, not a request ID. A later request may
legitimately replace that key with a different span.

HA accepts an optional `Idempotency-Key` header on mutating public requests. The
replicated deduplication key is `(authenticated principal, route, idempotency key)`.
The table stores the canonical request digest and committed response:

- the same key and digest returns the stored outcome;
- the same key with a different digest returns `409 idempotency_conflict`;
- entries have a documented bounded retention period and are included in snapshots.

Without an idempotency key, retry behavior remains at least once. Span upserts converge
through last-write-wins semantics, but retrying an annotation after an ambiguous timeout
may append another annotation. Traza does not claim exactly-once effects in that case.

### Commit and response sequence

A mutation follows this sequence:

1. The leader authenticates, parses, validates, canonicalizes, and stages any bounded
   chunks.
2. It appends application entries to its durable Raft log.
3. Followers validate cluster, version, prefix, sizes, and checksums and acknowledge
   only after the consensus library's required durable-write boundary.
4. Raft commits the entry on a voting majority.
5. The leader's single apply worker applies committed commands in index order.
6. The leader returns success only after the relevant commit is query-visible locally.

The `WriteBuffer` is a local materialized view, not the HA acknowledgment or recovery
boundary. A success is recoverable because the command is committed in the Raft log.
The leader does not need to create one segment per request.

### Applied-index recovery contract

Traza tracks three positions:

- `commit_index`: committed by Raft;
- `visible_applied_index`: reflected in this process's query-visible state;
- `durable_applied_index`: recoverably materialized in the active engine generation.

The apply worker may advance the visible index after putting a span into the write
buffer, but it advances the durable index only after every effect through that index is
durable. On restart it replays `(durable_applied_index, commit_index]`.

Every application command is idempotent by Raft log ID:

- staged span batches and payload chunks use log/batch IDs;
- annotation records persist their originating log ID and reject replay duplicates;
- retention persists the plan ID, cutoff, and deletion digest and is safe to repeat;
- payload sealing verifies the same digest;
- flush barriers record the highest sealed log index.

An engine checkpoint atomically publishes the segment and satellite-store state through
an index, then updates the generation manifest's durable applied index. A crash before
manifest publication replays commands; a crash after publication starts after them.
Raft log compaction never removes entries newer than the last durably installed
snapshot.

This requires new, narrow engine APIs before networked HA work begins:

```text
apply_committed(log_id, command)
begin_snapshot(applied_index) -> SnapshotView
install_generation(validated_generation)
durable_applied_index()
```

The exact Rust signatures are implementation details; their crash and locking semantics
are not.

## Logical retention versus physical compaction

Consensus decides logical visibility. A committed `RetentionPlan` is the only
distributed expiration decision. It is applied in log order and can be replayed.

Segment creation, merging, and supersede-journal recovery remain local. Two replicas may
have different segment counts or file names while representing the same logical spans.
The supersede journal never appears in the Raft command log and never advances a Raft
applied index. A local compaction failure makes that node unhealthy if it cannot continue
to represent the committed prefix; it does not change cluster history.

This choice has an API consequence: `/v1/stats` is a node-local diagnostic because its
physical record, buffer, segment, and byte counts can differ across replicas and after
failover. In HA mode its response adds node ID, role, term, and applied index. It is not
advertised as a linearizable cluster total. Logical analytics such as `/v1/stats/llm`
remain governed by the read contract below.

## Request routing and consistency

The first implementation does not proxy or redirect client requests through followers.
A follower receiving a mutation returns retryable `503` JSON with
`error: "not_leader"` and a configured public leader hint when known. No auth failure is
redirected. Unknown leader, no quorum, recovery, and incompatible-version states also
return explicit retryable errors.

| Surface | Default contract in HA mode | Optional follower behavior |
|---|---|---|
| Mutating routes | Leader only; quorum commit plus leader visibility before success | Rejected with `not_leader` |
| `/v1/spans`, `/v1/traces/*`, sessions, LLM analytics, annotations | Linearizable leader read at one applied index | `consistency=stale`, labeled with term and applied index |
| `/v1/export` | Leader ReadIndex followed by a pinned `SnapshotView`; all pages come from that view | Disabled initially |
| `/v1/payloads/*` | Leader read pinned against deletion for the operation | Stale only when explicitly requested |
| `/v1/stats` | Local physical diagnostic, labeled with node and applied index | Always local |
| Dashboard shell, liveness, metrics | Local and non-authoritative | Always local |

A ReadIndex barrier proves leadership but does not itself create an engine snapshot.
After the barrier, the node waits until `visible_applied_index` reaches the returned
commit index. Point reads acquire a state-machine read gate that cannot interleave with
multi-store application. Export pins an immutable `SnapshotView` at that index rather
than re-querying a changing store page by page. Its existing completion/count trailers
remain mandatory, and the response additionally reports the snapshot applied index.

Follower responses requested with `consistency=stale` include
`X-Traza-Consistency: stale`, `X-Traza-Term`, and `X-Traza-Applied-Index`. They never
claim read-your-writes or bounded staleness unless a separately tested maximum-lag
contract is configured.

## Election, membership, and fencing

Followers start an election after a randomized timeout without valid leader contact.
Terms and votes are persisted before the corresponding protocol response. A candidate
must have an up-to-date log and a voting majority. A higher term immediately fences an
older leader.

A newly elected leader commits a current-term no-op and replays through its commit index
before accepting mutations or linearizable reads. A minority-side former leader cannot
commit and fails write readiness after quorum checking detects loss of contact. Lease
reads are out of scope until a clock model and fault tests justify them.

Membership changes use learners and joint consensus:

1. add and authenticate a node as a non-voting learner;
2. catch it up and validate its state;
3. enter joint old/new voter configuration;
4. commit the new configuration;
5. remove obsolete authorization.

A two-voter deployment cannot survive either voter failing and is rejected by the
production configuration validator. Three voters are the minimum production topology.
Five voters are supported when two-failure tolerance justifies the latency and cost.

## Snapshots and catch-up

### Snapshot contents

A snapshot is built at one committed and durably applied index. Its manifest contains:

- cluster ID, command schema, engine format, last included log ID, and membership;
- every visible v2 segment plus length and cryptographic digest;
- annotation records and their originating log IDs;
- every sealed payload referenced by live spans plus length and digest;
- the replicated idempotency outcome table;
- greatest applied retention cutoff and flush barrier;
- active-generation metadata and a manifest digest.

The in-memory primary-key map and analytics caches are derived and are rebuilt after
installation. Embedded segment indexes are validated by the normal segment opener but
are not independent authority.

### Consistent creation

`begin_snapshot(index)` waits until the durable applied index reaches `index`, seals any
required buffer state, and returns a generation-pinned view. It does not copy a live
mutable annotation log or payload tree without coordination. Snapshot work may release
the apply gate after the generation is pinned; immutable files may then be hard-linked
or copied into the snapshot staging directory.

### Transfer and installation

Transfer is authenticated, length-bounded, chunked, checksummed, and resumable or safely
restartable. A receiving node remains a learner or recovering voter and does not serve
reads while installing.

Installation proceeds as follows:

1. download into a unique `incoming/` directory;
2. verify cluster ID, versions, manifest, every file length and digest, and every segment;
3. fsync files and the incoming directory;
4. rename the completed directory into a new `generations/<id>`;
5. write and fsync a temporary `CURRENT`, atomically replace `CURRENT`, and fsync `data/`;
6. open a new `Store` against the selected generation and rebuild derived indexes;
7. publish the new store handle, then resume log replay at the next index;
8. retire the old generation only after all readers and exports release it.

A crash exposes either the old complete generation or the new complete generation.
Consensus metadata is never swapped with the engine generation. Unsupported atomic
rename or directory-sync behavior disqualifies a platform until an equivalent tested
protocol exists.

Log truncation is permitted only at or below a snapshot index after the snapshot is
durable under the configured recovery policy. A slow follower cannot retain unbounded
leader disk: per-peer byte windows, snapshot concurrency limits, log-size alerts, and an
operator-visible eviction policy are required.

## Failure and recovery behavior

The system distinguishes liveness from readiness. A process can be live while it is not
eligible for client traffic. Write readiness requires current leadership, recent quorum
confirmation, compatible versions, and a working apply path. Read readiness additionally
requires recovery through the advertised applied index.

Consensus-log framing and persistence belong to the selected Raft storage adapter, not
to `WriteBuffer` or segment output. A follower acknowledges a log append only after a
complete validated entry and required hard state are durable in the ordering required by
the consensus library. Torn log records are recovered by that adapter to the last valid
boundary. Partially written segment, annotation, payload, or generation files are
recovered by their state-machine idempotency and generation protocol and never count as
Raft commitment.

Disk-full, permission, checksum, incompatible-version, and fsync failures remove a node
from readiness. The node does not continue with memory-only consensus state. If safe
local recovery is impossible, it quarantines the generation and requests a snapshot
rather than guessing.

Loss of quorum is not ordinary failover. Forced recovery is an explicit destructive
procedure that:

- names the chosen last-known state and possible acknowledged-data risk;
- proves or requires operator confirmation that the old cluster cannot return;
- issues a new cluster identity and peer credentials;
- preserves the old data for audit;
- never appears as an automatic promotion.

## Security

Client bearer-token authorization remains in force on every node. Peer credentials are
separate and cannot be used as client credentials. Voting, append, snapshot, membership,
leadership-transfer, and recovery RPCs require the exact peer authorization.

Peer messages are authenticated before expensive allocation, then checked for cluster,
node, version, term, size, and digest. Snapshot paths are generated locally. Rate,
connection, in-flight byte, log, staging, and snapshot limits prevent one peer from
exhausting a node.

Logs and metrics include node, cluster, role, term, index, peer, and operation kind but
never bearer tokens, private keys, request bodies, payload bytes, or replicated command
content. Detailed topology and lag require administrative authorization.

Certificate rotation uses an overlap window committed in membership/configuration state:
install new trust, rotate node credentials, confirm every voter, then remove old trust.
Removing a node revokes membership authorization immediately even if certificate expiry
is later.

## Compatibility and migration

HA is opt-in. With no cluster configuration, v0.13 standalone behavior, file layout,
authentication, OTLP mapping, dashboard, export trailers, TTL scheduling, and public
responses remain unchanged.

HA introduces these explicit API extensions:

- optional `Idempotency-Key` on mutations;
- retryable structured `not_leader`, `no_quorum`, and `recovering` errors;
- `consistency=stale` and applied-index response metadata;
- node/role/applied-index metadata on the already local `/v1/stats`;
- snapshot applied index on export responses.

`POST /v1/flush` remains meaningful: a committed `FlushBarrier` forces the leader's
materialized state through that index into a durable local segment before success.
Quorum durability already comes from Raft; the endpoint does not claim that every
follower has compacted or produced an identical segment layout.

Standalone-to-cluster migration requires:

1. a verified standalone backup;
2. stopping and fencing the standalone writer;
3. importing its complete engine state as generation zero at snapshot index zero;
4. creating one cluster identity and initial voter configuration;
5. joining empty learners through snapshot transfer;
6. permitting cluster writes only after the initial voter set is healthy.

Rollback is allowed before the first cluster mutation. After that point, rollback
requires a versioned export/import procedure accounting for every committed state
surface.

Command and consensus metadata versions are separate from segment format versions.
During rolling upgrades, a leader emits only command versions understood by every
voter. Unsupported nodes remain learners or reject join. The supported version and MSRV
matrix is release evidence, not prose-only intent.

## Operations and observability

Configuration distinguishes cluster ID, node ID, advertised client and peer addresses,
bootstrap versus join, voter role, heartbeat and election timings, log and snapshot
limits, TLS identity, engine generation root, and compatibility policy. Invalid or
ambiguous bootstrap combinations fail startup.

Required metrics include:

- role, term, leader and membership;
- commit, visible-applied, and durable-applied indices;
- per-peer match index, byte lag, and last contact;
- elections, quorum status, and leadership changes;
- proposal, quorum, apply, and checkpoint latency;
- log and staging bytes;
- snapshot build, transfer, install, and failure counts;
- deduplication hits and conflicts;
- stale-term, not-leader, corruption, fsync, and quarantine events.

Operators need tested runbooks for bootstrap, add/remove/replace, maintenance, leadership
transfer, backup, restore, certificate rotation, rolling upgrade, downgrade where
supported, and forced quorum-loss recovery. Every destructive operation states
preconditions, quorum impact, rollback boundary, and audit artifacts.

The product objective is RPO 0 for quorum-acknowledged mutations after one voter failure
and RTO under 10 seconds for leader failover on the reference deployment. Election
timeouts remain configurable; no environment is promised that bound until its latency
and storage assumptions pass the fault suite.

## Acceptance evidence

| ID | Requirement | Required executable evidence |
|---|---|---|
| HA-001 | One write leader | Three real processes expose exactly one write-ready leader per term; follower mutations never succeed |
| HA-002 | Acknowledged-state survival | Commit spans, annotations, and payloads; kill the leader at every response boundary; the successor returns every acknowledged value |
| HA-003 | Split-brain prevention | Partition the old leader into a minority; zero minority successes; heal and prove one converged committed history |
| HA-004 | Complete state replication | Exercise every command in the state inventory and compare logical results, payload bytes, annotations, and retry outcomes after failover |
| HA-005 | Applied-index crash safety | Kill before effect, after effect, before durable-index publication, and after publication; restart without omission or duplication |
| HA-006 | Request identity | Lost response plus same key returns the original result; same key/different digest fails; no-key annotation retry remains documented at-least-once |
| HA-007 | Linearizable reads | History checker covers successful, failed, timed-out, and retried reads/writes during partitions and leader kills |
| HA-008 | Export snapshot | Mutate concurrently with a multi-page export; output exactly matches its advertised applied-index snapshot and completion trailer |
| HA-009 | Physical-layout independence | Replicas compact at different times yet return identical logical results; `/v1/stats` remains correctly node-local |
| HA-010 | Snapshot completeness | Catch up after log truncation and verify spans, annotations, payloads, dedupe outcomes, retention, and all manifest digests |
| HA-011 | Atomic installation | Kill at every installation step; restart selects exactly the old or new complete generation |
| HA-012 | Membership safety | Add learners, promote, remove, and replace while writing; model-check quorum intersection and reject removed credentials |
| HA-013 | Security | Reject missing, client, wrong-cluster, wrong-node, expired, and removed-node peer credentials without secret leakage |
| HA-014 | Resource bounds | Slow peer and oversized request tests prove bounded memory, staging, log, and snapshot concurrency |
| HA-015 | Version skew | Supported rolling upgrade under traffic succeeds; unsupported command or metadata versions fail closed |
| HA-016 | Standalone compatibility | Complete current `./ci.sh` passes unchanged with HA disabled |
| HA-017 | Recovery objectives | Repeated fault runs report RPO and RTO distributions; no retry is hidden and no fixed sleep substitutes for an eventual assertion |

The harness uses production protocol, persistence, request-routing, and snapshot paths.
It records client history, node logs, terms, commit/applied indices, process exits, and
final state. Mocked consensus or test-only success flags are not sufficient evidence.
Flaky retries are reported with every attempt.

## Implementation sequence

### Phase zero: close architecture gates

- inventory and encode every v0.13 mutation;
- implement idempotent log-ID-aware annotation, payload, retention, and batch staging;
- implement engine checkpoint, generation install, and pinned read/export views;
- spike and approve the consensus library, runtime, TLS, dependency set, and MSRV;
- model command application, membership, and snapshot invariants;
- publish exact HTTP consistency and retry contracts.

No networked HA mode is exposed in phase zero.

### Phase one: durable single-node state machine

Run the selected consensus storage and apply adapter as one voter. Prove term, vote, log,
visible/durable applied indices, restart replay, large-batch staging, and snapshots under
fault injection. This phase claims recoverability, not availability.

### Phase two: three-node replication

Add authenticated transport, election, quorum replication, leader-only mutations,
ReadIndex, fencing, readiness, and real three-process partition tests. No membership
changes or follower reads are exposed yet.

### Phase three: snapshots and operations

Add learner catch-up, generation installation, log compaction, joint-consensus
membership, leadership transfer, backup/restore, metrics, and operator runbooks.

### Phase four: compatibility and qualification

Add supported rolling upgrades, migration, explicit stale reads, extended chaos and soak
tests, linearizability analysis, security review, RPO/RTO measurement, and independent
release review.

## Remaining decisions

These are explicit blockers, not details silently delegated to implementation:

1. Approve or reject the evaluated OpenRaft patch and storage adapter.
2. Approve the async runtime and TLS provider, including platform and MSRV support.
3. Fix the deduplication retention default and authenticated-principal namespace.
4. Fix entry, staging, snapshot, log, and peer-flow-control limits from measured data.
5. Define the supported rolling-version matrix.
6. Define certificate provisioning and rotation UX.
7. Define the supported filesystem/platform crash-consistency matrix.
8. Review forced quorum-loss recovery independently.

The architecture becomes implementation-ready only when phase-zero decisions are
recorded and their executable oracles exist. Completion of implementation phases is
evidence for review, not self-approval.
