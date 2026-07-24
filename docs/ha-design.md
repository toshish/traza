# Traza High-Availability Design

## Document status

This is a design proposal, not shipped behavior. It describes a proposed
high-availability architecture for Traza and the evidence an implementation
would need to provide. It does not assert that HA behavior, HA tests,
operational readiness, or production deployment support currently exists.
Requirement identifiers in this document are traceability labels for review.

## Purpose and scope

Leg 6 adds a design for making a Traza deployment tolerate loss of its active server
without accepting split-brain writes, corrupting persisted data, silently losing
acknowledged writes, or changing the established client-facing behavior unnecessarily.
The design covers leader election, replicated durable state, failure detection,
failover, fencing, recovery, observability, security, compatibility, and reproducible
acceptance evidence.

The proposal preserves the carried-forward storage engine as the authority for record
encoding, query semantics, segment validation, retention, and startup recovery. HA is a
coordination and replication layer around that engine, not a second independent storage
format. A replica may serve as leader only after it has obtained an exclusive term and
has recovered all state committed in that term's predecessor history.

In scope:

- a single writable leader and one or more followers;
- quorum-based election and commitment;
- replication of ordered logical storage mutations and required metadata;
- deterministic replay through the existing engine persistence boundary;
- failover with explicit fencing and bounded detection behavior;
- follower catch-up, snapshot installation, and replacement-node recovery;
- compatibility with authentication, OTLP ingestion, dashboard access, expiration,
  and segment format validation already represented in the v0.9.0 tree;
- operator-visible health, role, term, lag, and recovery state;
- tests and fault-injection oracles that distinguish real behavior from stubs.

Out of scope for this design artifact:

- implementing or shipping the proposed protocol;
- multi-leader or conflict-free replicated writes;
- cross-cluster federation;
- automatic geographic placement policy;
- changing closed earlier-leg formats or acceptance contracts;
- claiming a particular deployment is ready for production use;
- tagging, publishing, or independently closing Leg 6.

## Terminology

**Node** is one Traza server process with a stable node identity and private durable
state. **Cluster** is the configured voting membership sharing one cluster identity.
**Leader** is the sole node permitted to propose client-visible mutations in a term.
**Follower** receives and durably records replicated entries. **Candidate** requests
votes during election. **Term** is a monotonically increasing election epoch persisted
before a vote or leadership action. **Log index** is the monotonically increasing
position of a replicated mutation. **Commit index** is the highest index known durable
on a quorum and therefore eligible for application. **Applied index** is the highest
committed entry reflected in the local Traza engine. **Quorum** means a strict majority
of the current voter configuration. **Fencing** is rejection of work from an obsolete
term or non-leader. **Snapshot** is a point-in-time transferable image with cluster,
term, membership, format, and last-included-index metadata.

An **acknowledged write** is one for which the public request returned success after the
corresponding entry became committed and locally applied. Receiving, buffering, or only
locally persisting a request is not an acknowledgment boundary. A **linearizable read**
is evaluated on the leader only after a quorum-confirmed leadership barrier and after
application through the barrier's commit index. A **stale read** is an explicitly opted
in follower read and cannot be represented as linearizable.

## Current Traza mechanisms

The sanctioned v0.9.0 tree is a single-node system. This section records integration
points visible in that carried-forward tree and must not be read as evidence that those
mechanisms already provide distributed coordination.

The library entry point in `src/lib.rs` exposes the storage engine and its request-facing
operations. The existing engine remains the application state machine. Its startup,
write, query, flush, and shutdown boundaries need to be wrapped rather than duplicated.
The successor implementation must inspect the concrete types and lock ordering before
introducing asynchronous replication callbacks; this design does not invent an existing
replication hook.

`src/segment_v2.rs` and `docs/segment-format-v2.md` define the carried-forward segment
representation and validation behavior. Replication does not rewrite segment bytes into
a new closed-leg format. Snapshot transfer must preserve or reconstruct valid v2
segments and must run the same validation used by ordinary startup. Corrupt snapshots
or replicated segment material fail closed.

`src/expiration.rs` performs retention or expiration work that mutates stored state.
Under HA, time-based mutation cannot run independently on every node. The leader alone
must decide an expiration mutation and append it to the replicated log; followers replay
that decision. This prevents clock skew from producing divergent data sets.

`src/otlp.rs` maps OTLP ingestion into engine writes. OTLP requests follow the same
leader, commit, idempotency, and response rules as any other write. A follower must not
silently accept and apply an OTLP write locally. It returns a documented not-leader
response or redirects only where the protocol and authentication policy permit it.

`src/auth.rs` supplies the existing authentication and authorization boundary.
Cluster-internal identity is additional to, and not a bypass around, client auth.
Replication, voting, snapshot, and administrative membership endpoints require mutual
node authentication and explicit authorization.

`src/dashboard.rs` and `src/dashboard.html` provide the existing dashboard surface. A
future HA status view may expose role and health, but it must not leak secrets, peer
credentials, bearer tokens, raw replication payloads, or sensitive error detail. The
existing dashboard behavior remains compatible when HA is disabled.

`src/bin/traza-server.rs` owns process startup and network lifecycle. It is the natural
composition point for cluster configuration, transport startup, readiness state,
graceful leadership transfer, and coordinated engine shutdown. `src/bin/bench.rs` is not
an acceptance oracle for correctness; throughput measurements cannot substitute for
fault tests.

The integration suites in `tests/auth.rs`, `tests/dashboard.rs`,
`tests/llm_semantics.rs`, `tests/otlp_conformance.rs`,
`tests/segment_format_v2_acceptance.rs`, `tests/server_on_engine.rs`, and
`tests/storage.rs` define carried-forward regression behavior. HA work must add focused
fault and cluster tests while continuing to pass the complete existing suite.

## HA architecture

### Components and responsibilities

Each node contains five proposed layers:

1. The client front end authenticates requests and classifies each operation as a read,
   write, administration operation, or local diagnostic.
2. The consensus module persists term, vote, membership, replicated log, and commit
   progress and implements leader election and quorum commitment.
3. The HA state-machine adapter converts an accepted mutation into a deterministic log
   command, applies committed commands to the existing engine in index order, and
   records deduplication results where retries require them.
4. The snapshot manager creates, validates, transfers, installs, and retires snapshots
   without exposing a partially installed engine state.
5. The health and operations layer reports role, term, leader identity, commit/applied
   indices, peer reachability, replication lag, snapshot progress, and readiness.

The protocol should use a well-specified consensus algorithm with Raft-equivalent safety
properties rather than an ad hoc heartbeat lock. The implementation may use a reviewed
library if its persistence and membership semantics satisfy this design. Choosing a
library does not waive protocol-level tests.

### Stable identity and bootstrap

Every node has an operator-provisioned stable node ID, cluster ID, peer address, client
address, and data directory. Node IDs cannot be inferred only from ephemeral addresses.
A node persists the cluster ID and refuses to join or restore data from another cluster.
Reusing a removed node's data directory or identity fails with a diagnostic unless an
explicit, validated recovery procedure authorizes it.

A new cluster is bootstrapped once with an explicit initial voter set. Starting several
empty nodes with independent bootstrap flags must not create several clusters that later
merge. Joining is performed through a committed membership operation. Discovery may
find peers, but discovery does not establish voting membership or trust.

### Request routing

Only the leader accepts writes. A follower responds with a machine-readable not-leader
result containing a leader hint only when known and safe to disclose. Automatic proxying
is optional and, if selected, must preserve authentication context, request identity,
timeout, body limits, and end-to-end error semantics. Redirect loops are prevented by a
hop marker or by clients connecting directly to the advertised leader.

Default reads are linearizable and therefore pass through the leader's read barrier.
Follower reads are disabled unless the caller explicitly requests stale semantics. An
explicit stale response includes enough metadata, such as applied index and observed
term, for diagnostics. Administrative state changes are replicated writes. Node-local
health and metrics may remain local.

## Replication model

### Replicated command log

The leader converts each mutating operation into a deterministic, versioned command.
Each entry contains cluster protocol version, term, index, command kind, canonical
payload, request/deduplication key when applicable, and integrity metadata. Entries must
not depend on follower wall clocks, random generation, unordered map iteration, or local
file names. Values that would otherwise be nondeterministic are selected by the leader
and included in the command.

An entry is appended durably on the leader before replication. Followers validate term,
predecessor index and term, command version, length, and checksum before durable append.
A follower acknowledges append only after the bytes and required metadata cross the
specified persistence boundary. The leader advances commit index only when the entry is
stored on a quorum and the consensus algorithm permits commitment. It then applies
entries in strict index order and responds success only after the requested command is
applied locally.

The exact fsync policy is part of correctness, not a tuning detail. A mode that reports
success before quorum durability must be named unsafe, must not be the default, and
cannot satisfy the acknowledged-write oracle.

### Application and idempotency

The state-machine adapter is the sole path from committed log entries to engine
mutation. Concurrent client requests may be proposed concurrently, but application is
serialized by log index or otherwise proven observationally equivalent. The adapter
persists the applied index atomically with, or recoverably relative to, the engine
mutation. On restart it can determine whether to replay an entry without duplicating
its effect.

Client retries across leader changes require a stable request ID for operations whose
repetition changes results. The replicated deduplication table maps client/request ID to
the committed outcome and has a bounded, documented retention policy. A retry with the
same identity and different canonical payload is rejected. If the existing public API
cannot carry such an identity, exactly-once effects are an unsupported requirement until
the API is extended; the system must document at-least-once retry behavior rather than
claim stronger semantics.

### Snapshots and catch-up

Log compaction requires a snapshot created at a committed applied index. Snapshot
metadata includes cluster ID, format and protocol versions, last included index and
term, membership configuration, content manifest, lengths, and cryptographic checksums.
Creation uses an engine-consistent view; copying live files without the engine's
consistency guarantee is invalid.

Transfer is chunked, authenticated, checksummed, resumable or safely restartable, and
written to a temporary location. Installation validates all metadata and segment
content before an atomic directory or engine-state swap. A crash at any transfer or
installation point leaves either the old valid state or the new complete valid state,
never a mixed state. After installation, replay begins at the next index. Obsolete logs
and snapshots are deleted only after the new state is durable and active.

## Failure detection and failover

Leaders send periodic heartbeats. Followers start an election after a randomized timeout
without valid leader contact. Timeout defaults and allowed ranges must be documented and
validated against expected network latency and storage stalls. A health endpoint is not
the election oracle; consensus messages and persisted terms govern authority.

Before voting, a node persists the new term and its vote. It grants at most one vote per
term and only to a candidate whose log is at least as up to date. A candidate becomes
leader only after a quorum elects it. On receiving a higher term, every role immediately
steps down and persists that term before further protocol action.

A newly elected leader appends or confirms a current-term barrier before serving
linearizable reads or acknowledging new writes. Uncommitted suffixes from an obsolete
leader may be overwritten according to consensus rules and must never have been exposed
as successful writes. Committed entries are retained by every future leader.

Network partitions preserve safety: only the majority side can elect and commit. A
minority-side former leader cannot acknowledge writes and fails readiness after losing
quorum. Lease optimizations may be added only with a documented clock model and tests;
term and quorum fencing remain authoritative.

Graceful shutdown first stops accepting writes, optionally transfers leadership to an
eligible caught-up voter, waits only for a bounded interval, flushes protocol and engine
state, and exits. Abrupt process termination, machine loss, packet loss, duplication,
reordering, and delayed disk completion are all required fault cases.

Readiness differs from liveness. A process is live when its local event loop can answer.
It is ready for writes only when it is the quorum-confirmed leader and its state machine
is operational. A follower may be ready for replication while not ready as a write
target. Unknown leader, snapshot installation, incompatible format, corrupt state, and
quorum loss produce explicit non-ready reasons.

## Consistency and correctness

The safety invariants are:

- at most one quorum-authorized leader can commit entries for a term;
- committed log entries have one command at each index and appear in every future
  leader's history;
- commands apply exactly in committed index order;
- no node serves a successful write before quorum durability and leader application;
- no default read observes state older than a completed acknowledged write when the read
  begins after that write;
- an obsolete term cannot mutate externally visible state;
- expiration and other background mutation are leader-proposed replicated commands;
- recovery never treats partial, corrupt, foreign-cluster, or incompatible data as valid;
- membership changes preserve an intersecting quorum during transition.

Memory ordering and Rust concurrency safety alone do not establish these invariants.
Protocol state transitions, durable-write ordering, engine locking, and response ordering
must be tested together. Locks must not be held across unbounded network awaits. Applying
an entry must not call back into proposal code and deadlock. Shutdown and snapshot swaps
must coordinate with active readers and the engine lifecycle.

Linearizability is the proposed default client contract. It is verified with concurrent
histories containing successful, failed, timed-out, and retried operations while leaders
are killed and links partitioned. The checker must reason about ambiguous timeout results
rather than deleting them from history. A model-based or established linearizability
checker is preferred over assertions about final row count alone.

Membership changes use joint consensus or an equivalent protocol that maintains quorum
intersection. Adding a voter first catches it up as a non-voter; it does not vote until
sufficiently current. Removing a leader causes transfer or step-down. A two-node cluster
cannot remain available after either node fails and must not be advertised as satisfying
one-failure availability. Three voters are the minimum recommended topology.


### Partial write handling

Partial write handling is a correctness boundary, not an implementation detail. A leader must never count a replica acknowledgement until that replica has durably persisted and validated the complete framed replication record. `WriteBuffer` output is transferred as length-delimited records carrying the term, log position, payload length, and checksum; short socket writes are retried from the remaining byte offset, while disconnects leave the record unacknowledged. A follower writes incoming bytes to a staging file, loops until the complete frame is present, verifies its declared length and checksum, calls the required durability primitive, and only then atomically advances its persisted match position and returns an acknowledgement. Bytes from a truncated frame are never exposed through the primary-key index, query path, segment manifest, or quorum accounting.

On restart, recovery scans only complete validated frames. A torn header, short payload, checksum mismatch, or partially persisted `SegmentWriter` output is truncated back to the last validated record boundary before replication resumes. The supersede journal follows the same rule: its intent and completion records are independently framed and checksummed, so a partial journal write cannot make an old primary-key value disappear without a durable replacement. If truncation cannot be performed safely, the replica enters a quarantined/catch-up state and requests a snapshot or retransmission rather than serving reads or voting with ambiguous state. Leaders treat any partial write, timeout, or lost acknowledgement as not committed; retry uses the same term and log position so followers can reject conflicts and recognize an already durable duplicate without applying it twice. This preserves the invariant that acknowledged quorum positions correspond only to complete durable records and that recovery never invents a committed prefix from torn bytes.

## Operations and recovery

### Configuration and observability

Configuration distinguishes cluster ID, node ID, advertised client and peer addresses,
initial bootstrap, join target, voter role, election and heartbeat timings, snapshot
thresholds, storage paths, TLS identities, and compatibility policy. Invalid combinations
fail startup. Environment, file, and command-line precedence must be deterministic and
secret values must be redacted from diagnostics.

Metrics include current role and term, known leader, commit and applied indices, per-peer
match index and lag, election count, quorum status, proposal latency, replication
latency, apply latency, snapshot bytes and duration, rejected stale-term messages,
not-leader responses, deduplication hits, and corruption failures. Labels must avoid
unbounded request or tenant cardinality. Structured logs include node, cluster, term,
index, and peer context but no authentication secrets or sensitive payloads.

Operators need documented procedures for initial bootstrap, adding and removing nodes,
planned maintenance, leadership transfer, replacing a failed node, restoring from backup,
rotating certificates, upgrading, downgrading where supported, and disaster recovery.
Every dangerous operation states preconditions and expected quorum impact.

### Backup and disaster recovery

A backup is derived from a validated snapshot at a committed index plus sufficient
metadata to restore cluster identity deliberately. Restoring one node from old data into
a live cluster as if current is forbidden. A restored cluster receives an explicit new
cluster identity unless the disaster-recovery procedure proves the old cluster cannot
return. This avoids two independently writable incarnations.

Loss of quorum is not automatically repaired by promoting arbitrary surviving data.
Operator-forced recovery is a distinct, destructive procedure that identifies the last
known committed state, records possible acknowledged-data risk, rotates cluster identity
or fencing credentials, and requires confirmation. It must not be presented as ordinary
failover.

Disk-full, permission, checksum, incompatible-version, and fsync failures force the node
out of write readiness. The implementation does not continue with memory-only consensus
state. Recovery diagnostics identify the failed path and operation without deleting the
only good copy.

### Required fault exercises

Focused integration tests run real server processes or production protocol components
with isolated durable directories. They must not replace persistence or elections with a
test-only success flag. Required scenarios include leader process kill, follower kill,
majority/minority partition, message delay and reorder, restart after append and before
apply, restart after apply and before response, disk-full or injected durable-write
failure at the production persistence boundary, snapshot interruption, corrupt snapshot,
rolling version skew, and expiration across failover.

Each scenario records client history, node logs, terms, commit/applied indices, and final
engine contents. Tests use bounded eventual assertions with diagnostic output instead of
unexplained sleeps. Repeated randomized tests supplement, but do not replace,
deterministic regression cases.

## Security considerations

Peer transport requires mutual authentication, confidentiality, hostname or node-ID
verification, and a trust policy separate from public client credentials. A valid client
token cannot invoke voting, append, snapshot, join, remove, transfer, or recovery RPCs.
Node certificates bind the authenticated identity to configured membership. Removed
nodes lose authorization even if an old transport credential remains temporarily valid.

All peer messages are length-limited and version-checked before allocation or decoding.
Snapshot paths are generated internally; peer-controlled names cannot escape temporary
storage. Checksums detect corruption but do not replace authenticated transport.
Replay-sensitive administrative requests include term, cluster identity, and request
identity. Rate and concurrency limits prevent a peer from exhausting memory, disk,
threads, or snapshot slots.

Existing client authentication remains in force on whichever node receives a request.
If proxying is implemented, it forwards a signed, narrowly scoped identity assertion or
re-authenticates through a defined mechanism; it never forwards reusable secrets in
logs or query strings. Leader hints disclose only configured public addresses.

Operational endpoints reveal minimal data by default. Detailed peer topology and lag
require administrative authorization. Metrics and logs redact tokens, key material,
request bodies, and replicated values. Backup and snapshot files receive permissions
and encryption controls equivalent to the primary data directory.

## Compatibility and migration

HA is opt-in. With no cluster configuration, a v0.9.0-compatible standalone deployment
continues to use the established engine, APIs, authentication, OTLP behavior, dashboard,
expiration behavior, and segment validation. HA configuration must not silently reinterpret
an existing standalone directory as a multi-node cluster.

Migration begins by taking and validating a backup, stopping the standalone writer,
initializing one cluster leader from the existing engine state at a defined snapshot
index, recording cluster metadata, then joining empty followers through snapshot transfer.
The old standalone process remains fenced and must not restart against the migrated data
directory. Rollback is allowed only before new cluster writes or through an explicit
export procedure that accounts for all committed data.

Protocol and command entries carry versions. During rolling upgrades, a leader proposes
only features understood by the active voting configuration. Incompatible nodes remain
non-voting or fail join clearly. The supported version matrix and order of upgrades must
be documented and tested. On-disk consensus metadata has its own version independent of
segment v2.

Public success and error semantics remain stable where possible. New not-leader,
no-quorum, recovering, and incompatible-version outcomes need documented HTTP or RPC
mapping and retry guidance. Existing auth failures must not become redirects. Existing
OTLP conformance and server-on-engine tests remain regression gates.

No migration may rewrite closed-leg documents or weaken their tests. If an HA command
cannot represent an existing engine operation deterministically, that operation is a gap
to resolve in successor work, not grounds for silently changing prior semantics.

<!-- leg6-grounded-architecture-comparison -->
## Non-goals

This proposal deliberately does not redesign the closed Legs 1 through 5, change the segment-v2 on-disk contract, replace OTLP or dashboard APIs, or reinterpret authentication and LLM-semantic behavior. It does not promise multi-region active-active writes, Byzantine-fault tolerance, transparent operation through arbitrary network partitions, zero-data-loss asynchronous replication, or automatic disaster recovery without an operator-provided quorum and durable storage. It also does not treat a shared filesystem, process supervision alone, or `DirectoryLock` alone as a distributed consensus mechanism. Existing single-node operation remains a supported compatibility mode. This document proposes future work; none of the HA behavior described here is asserted to exist in the carried-forward tree.

## Grounded Current-State Persistence Boundaries

The sanctioned v0.9.0 engine has several concrete mechanisms that constrain an HA design:

- `WriteBuffer` is the in-process mutable ingestion boundary. Data accepted only into a leader's `WriteBuffer` cannot be considered replicated or failover-safe. A future leader must acknowledge a write only after the replicated log policy is met, then apply the committed entry to its local `WriteBuffer` and normal storage path.
- `segment_v2` is the durable segment format and remains the local immutable-data representation. Replication should carry logical committed operations and verified immutable segment artifacts rather than allowing multiple processes to append concurrently to the same segment file.
- The supersede journal records replacement/supersession intent around segment transitions. Its ordering and recovery semantics must be represented in the replicated state machine so failover cannot expose both an obsolete segment and its replacement, lose a completed supersession, or finish an uncommitted one.
- The `(trace_id, span_id)` trace/span primary key defines idempotent identity for span writes. Retries across an uncertain failover boundary must converge on the same logical record rather than create a second span; conflicting payload policy must be deterministic and replicated.
- `DirectoryLock` fences concurrent local owners of a data directory. It protects one node's files but neither establishes cluster leadership nor fences a stale leader from remote clients. Each replica therefore owns a distinct directory and acquires its own `DirectoryLock`; distributed fencing is supplied separately by quorum term and commit rules.

These are current mechanisms and invariants used as integration constraints, not evidence that replication is already implemented.

## Architecture Options Comparison

### Architecture Option 1: Shared Storage With Active-Passive Processes

Two server processes could point at one shared data directory while an external manager chooses the active process. This has a small conceptual diff, but it is rejected as the primary design. `DirectoryLock` permits only a local directory owner and does not reliably fence hosts across all network filesystems. Cache coherence, partial segment writes, supersede journal transitions, and lock behavior after partitions would depend on filesystem semantics outside Traza's control. It also leaves acknowledged `WriteBuffer` contents vulnerable when the active process dies.

### Architecture Option 2: Primary-Backup File Shipping

A primary could retain the existing write path and periodically ship closed `segment_v2` files plus supersede journal material to one or more backups. This is useful later for snapshots and catch-up, and it preserves immutable artifacts efficiently. By itself it cannot provide the required correctness boundary for recently acknowledged writes: an open `WriteBuffer` and partially completed supersession are not necessarily represented by a shipped segment. Promotion also needs an external authority and a durable ordering point to prevent split brain. This option is therefore retained only as a bulk-transfer optimization beneath a replicated protocol.

### Architecture Option 3: Quorum Replicated Logical Log

A fixed-membership Raft-style state machine assigns each mutation a monotonically ordered `(term, index)` and commits it after durable acknowledgement by a voting majority. Each replica has a private `DirectoryLock`-protected data directory and independently applies committed operations through the existing engine boundaries. The log covers span upserts keyed by `(trace_id, span_id)`, retention/expiration decisions that affect visible state, and supersede journal state transitions. Closed `segment_v2` files can be transferred as checksummed snapshots or immutable artifacts, but their installation is authorized by a committed manifest entry.

This option gives Traza an internal leader election, an explicit acknowledgement oracle, stale-leader rejection, deterministic recovery, and a single ordering source. Its costs are a larger implementation surface, quorum latency, membership operations, and the need to specify deterministic application around existing background work.

### Architecture Option 4: External Consensus Service

Traza could lease leadership and publish metadata through an external service such as etcd while file shipping carries data. This can provide strong fencing if every mutation validates a revision or lease, but introduces a mandatory operational dependency and still requires a durable data replication protocol. A metadata lease alone cannot recover acknowledged `WriteBuffer` entries. It remains a possible deployment adapter, not the recommended correctness core.

## Recommended HA Architecture

The recommended future direction is Architecture Option 3: one voting leader and quorum-replicated logical log, with file/snapshot transfer from Option 2 for efficient catch-up. Each node consists of a transport/authentication boundary, consensus module, durable log and metadata store, deterministic apply worker, and the existing Traza engine operating in a node-private directory. Readers default to the leader or use an explicit linearizable read barrier; followers may serve explicitly labeled stale reads only if the governing specification permits that mode.

The consensus term is the distributed fencing token. A node may accept mutations only while it is leader in the current term and can contact a quorum. The mutation response includes stable operation identity and is successful only after the entry is durably committed. Loss of quorum makes the old leader reject new writes even if it still holds its local `DirectoryLock`. A promoted follower first completes election, establishes a current-term committed entry/read barrier, replays committed unapplied entries, and only then advertises write readiness.

## Replication Protocol

The proposed replication protocol uses authenticated, versioned peer messages and persistent consensus metadata. Every request carries cluster ID, sender node ID, protocol version, term, and message identity. Vote requests include the candidate's last log term/index; append requests include the preceding term/index, an ordered entry batch, leader commit index, and current fencing term. Receivers reject the wrong cluster, unsupported protocol, stale term, inconsistent prefix, invalid authentication, or malformed size/checksum before mutating durable state.

A client mutation follows this exact sequence:

1. The leader authenticates and validates the request, derives an idempotency identity from the operation and, for spans, the `(trace_id, span_id)` primary key, and appends a typed log entry to its durable local log.
2. The leader sends the entry to voting followers. Each follower verifies the prefix and durably flushes the entry before acknowledging it; merely buffering it in memory is not an acknowledgement.
3. After a voting majority, including the leader, has durably stored the entry, the leader advances the commit index according to the consensus term rule. Only this committed point permits a successful client response.
4. Every replica applies committed entries in index order. Span mutations enter the local `WriteBuffer` and existing persistence path deterministically. Supersession operations drive the supersede journal in the same committed order. Apply progress is stored so restart can safely repeat an entry.
5. Duplicate requests return the prior committed result or deterministically reapply an idempotent operation. A timeout before the client observes a response is an unknown outcome, so retry with the same identity is required.

Followers that are behind stream missing log entries. If retained log history is insufficient, the leader creates a snapshot at a committed index, including a manifest of visible `segment_v2` artifacts, primary-key/index state required for deterministic reads, supersede journal state, and integrity hashes. The follower downloads into a temporary location, verifies cluster ID, format version, index/term, lengths, and hashes, atomically installs it while holding its local `DirectoryLock`, and resumes log replay after the snapshot index. Interrupted or corrupt transfers remain non-visible and are retried; they never replace the last valid local state.

Flow control bounds outstanding bytes and entries per follower so a slow replica cannot exhaust the leader. A quorum may continue while one follower catches up, but the leader must step down or reject writes when it cannot maintain a majority. Log truncation is allowed only after the snapshot is durable on enough nodes to preserve the configured recovery guarantee. Membership changes use joint consensus rather than independent configuration edits; removing enough voters to erase quorum durability is rejected.

Protocol compatibility is negotiated before replication. Unknown required entry types stop application and prevent promotion; they are not silently skipped. Rolling upgrades require an overlap version in which all voters understand the emitted entry set. Encryption and mutual peer authentication bind a node identity to cluster membership, and authorization separates peer replication, membership administration, snapshot transfer, and client access.

## Consistency Consequences of the Recommendation

Successful writes are linearizable at the committed log index, while local application may trail commitment only behind an internal readiness barrier. A leader must not report a committed write as query-visible until its own apply index reaches that commit, and a promoted node must not serve until replay reaches the required index. Linearizable reads use a current-term quorum confirmation/read index and wait for local apply. Any follower-read mode must expose its applied index and be documented as potentially stale.

The `(trace_id, span_id)` primary key and stable request identity provide retry idempotence, but they do not by themselves settle conflicting payloads. The state-machine entry must encode a deterministic conflict rule matching the sanctioned single-node behavior or explicitly reject a mismatch. Time-based expiration cannot depend independently on each node's wall clock; the leader must replicate the expiration decision or a deterministic logical cutoff. Segment creation, compaction, and supersede journal completion may differ physically by node only where observable query state remains identical and snapshot manifests remain verifiable.

The design chooses consistency over write availability during a partition: only a side with a voting majority can elect a leader and commit. Minority nodes reject writes and must not self-promote. This is the necessary split-brain boundary; DNS, load balancers, process managers, and `DirectoryLock` are routing or local-safety aids rather than substitutes for it.

## Requirements traceability

The following matrix maps the mechanically identifiable normative requirements from
this design. Status is **proposed** unless a gap is stated. “Proposed” means this
design supplies an architecture and oracle; it does not mean runtime behavior exists.
Independent review must compare every entry with the source specification and correct
any interpretation mismatch.

| ID | Requirement interpretation | Proposed mechanism | Exact acceptance evidence / oracle | Status or gap |
|---|---|---|---|---|
| TRACE-001 | Define an HA deployment with one active writer and replicas. | Quorum-elected single leader; followers reject writes. | Start at least three real nodes; assert one write-ready leader and no successful follower write in each observed term. | Proposed. |
| TRACE-002 | Preserve acknowledged writes through failover. | Acknowledge only after quorum durability and leader apply. | Write uniquely identified records, wait for success, kill leader without graceful shutdown, elect a successor, and query every acknowledged record; repeat across durability crash points. | Proposed; persistence boundary needs implementation review. |
| TRACE-003 | Prevent split brain during partitions. | Majority quorum, persisted votes and terms, stale-term fencing. | Partition old leader into a minority; concurrent writes on both sides must show zero minority successes while majority commits; heal and verify one converged history. | Proposed. |
| TRACE-004 | Detect failure and perform bounded automatic failover. | Randomized election timeout, heartbeat, readiness transition. | Kill leader and measure from last valid heartbeat to new leader readiness under the configured bound; collect election timeline and repeat without fixed sleeps. | Proposed; exact bound follows governing specification/configuration. |
| TRACE-005 | Maintain a consistent ordered replicated history. | Term/index log matching, quorum commit, ordered state-machine apply. | Inject duplication, loss, reorder, and delay; compare committed index/term hashes and final engine results across caught-up nodes. | Proposed. |
| TRACE-006 | Fence obsolete leaders and stale requests. | Higher-term step-down, leader barrier, term checks on all mutation paths. | Pause old leader, elect and commit on majority, resume old leader, then prove all old-term proposals and background mutations are rejected. | Proposed. |
| TRACE-007 | Provide correct read semantics across failover. | Leader read barrier; stale follower reads only by explicit opt-in. | Run a linearizability checker over concurrent reads/writes and leader failures; separately verify follower reads are rejected by default and labeled when opted in. | Proposed. |
| TRACE-008 | Replicate all state-changing operations, including maintenance work. | Deterministic versioned commands; leader-proposed expiration. | Exercise normal ingestion, OTLP, expiration, and administrative mutations; fail over after each and compare query-visible state and applied indices. | Proposed; full mutation inventory requires source-level review. |
| TRACE-009 | Recover cleanly after process or machine restart. | Durable term/vote/log/applied metadata and idempotent replay. | Terminate at each append/commit/apply/response crash point, restart from the same production data directory, and verify no lost committed or duplicate mutation. | Proposed. |
| TRACE-010 | Catch up lagging or replacement replicas. | Incremental log replication followed by validated snapshot installation when compacted. | Isolate follower through log compaction, reconnect, observe snapshot plus tail replay, then compare engine state and commit/applied indices. | Proposed. |
| TRACE-011 | Detect corruption and avoid partial snapshot activation. | Manifest checksums, segment validation, temporary transfer, atomic installation. | Corrupt each metadata and payload class and kill during transfer/install; restart must retain old valid state or activate complete new state and must never become ready on corrupt state. | Proposed. |
| TRACE-012 | Support safe membership changes. | Catch-up as non-voter and joint-consensus voter transitions. | Add and remove nodes while writing and during induced failure; model-check quorum intersection and verify removed nodes cannot vote or commit. | Proposed; consensus implementation choice remains open. |
| TRACE-013 | Expose actionable health and HA observability. | Separate liveness/readiness plus bounded-cardinality role, term, lag, quorum, and snapshot metrics. | Query each node in leader, follower, candidate, partitioned, recovering, corrupt, and incompatible states; assert exact readiness and metric transitions. | Proposed; metric names need specification. |
| TRACE-014 | Secure cluster-internal communication and administration. | Mutual TLS/node identity, membership authorization, limits, redaction. | Attempt every peer/admin RPC with no credential, client credential, wrong-node credential, removed-node credential, and valid credential; inspect logs and metrics for secret leakage. | Proposed; credential provisioning/rotation remains open. |
| TRACE-015 | Retain standalone and existing API compatibility. | HA opt-in wrapper around existing engine and request surfaces. | Run the full v0.9.0 workspace test suite with HA disabled; run existing auth, OTLP, dashboard, storage, segment, semantics, and server tests unchanged. | Proposed; no earlier-leg behavior may be weakened. |
| TRACE-016 | Define safe migration and version interoperability. | Explicit stopped-writer import, protocol versions, rolling feature gate. | Migrate a populated standalone fixture, verify all data, perform supported rolling upgrades under traffic, reject incompatible joins, and exercise documented rollback boundary. | Proposed; supported version matrix is an open decision. |
| TRACE-017 | Make retry and duplicate behavior explicit. | Replicated request-ID deduplication and payload conflict check. | Drop the response after apply, retry through a new leader with same ID, and assert one effect and same result; same ID/different payload must fail. | Gap if public mutation APIs cannot carry stable request identity. |
| TRACE-018 | Provide operator recovery without unsafe automatic promotion. | Validated backups, replacement flow, explicit forced quorum-loss recovery. | Restore into an isolated new cluster identity, prove old-cluster messages are rejected, and require confirmation for forced recovery while reporting potential data loss. | Proposed; UX and authorization require review. |
| TRACE-019 | Supply focused real-behavior HA tests with no shortcuts. | Multi-process/component fault harness using production persistence and protocol paths. | Execute deterministic kill, partition, reorder, disk, snapshot, restart, expiration, security, and compatibility scenarios and retain histories and diagnostics. | Proposed; harness does not yet exist. |
| TRACE-020 | Preserve full-workspace quality and provide reproducible review evidence. | Existing regressions plus focused HA suite and recorded Git identity. | From a clean sanctioned descendant run formatting/checks required by the repository, `cargo build --workspace`, and `cargo test --workspace`; record exact commands, complete exit status, `git rev-parse HEAD`, `git rev-parse HEAD^{tree}`, and scoped diff. | Proposed; evidence can be claimed only after successor implementation and independent run. |

### Evidence record format

A successor implementation report should provide, for every requirement, the exact
command, environment assumptions, exit code, relevant parsed result, and artifact path.
It should include the sanctioned seed commit, implementation commit, tree ID, clean or
fully enumerated worktree status, compiler version, and operating system. A timeout is a
failed or absent result, not evidence of success. Flaky retries must be reported with all
attempts. Unsupported requirements remain explicit gaps.

The minimum final command set is expected to include repository-prescribed formatting or
lint commands where present, `cargo build --workspace`, `cargo test --workspace`, focused
HA integration invocations, and the specification's exact commands. The source
specification controls if it names a stricter command. Unit tests that mock away quorum,
durable storage, transport failure, or production request routing cannot be the sole
oracle for distributed requirements.

## Implementation phases

Phase one introduces durable protocol metadata and a deterministic state-machine adapter
behind an opt-in configuration, with no claim of availability. It establishes crash-safe
term, vote, log, commit, and apply behavior plus restart fault tests.

Phase two adds authenticated peer transport, election, replication, leader request
routing, fencing, and linearizable read barriers. It adds three-node partition and
leader-kill tests before exposing the mode outside development.

Phase three adds snapshots, compaction, lagging-replica recovery, membership changes,
and operational APIs. Snapshot corruption and crash-atomicity tests gate this phase.

Phase four integrates all mutating surfaces, including OTLP and expiration, adds
migration and rolling-upgrade behavior, and executes the complete carried-forward
regression suite.

Phase five performs extended fault injection, linearizability analysis, soak testing,
security review, and operator runbook exercises. Completion of phases is evidence for
review, not self-approval; independent closure remains a separate gate.

## Risks and open questions

1. **Engine transaction boundary:** The existing engine may not expose an atomic way to
   couple applied index with every mutation. The implementation must design recoverable
   replay or add a narrowly scoped metadata transaction without changing stored record
   semantics.
2. **Mutation inventory:** Every path that changes durable or query-visible state must be
   classified. An omitted background task can cause replica divergence.
3. **Request identity:** Existing APIs may lack stable idempotency keys. Exactly-once
   retry effects remain unsupported until an authenticated, bounded identity contract is
   available.
4. **Consensus dependency:** Building versus adopting a consensus implementation affects
   audit scope, transport integration, storage ownership, licensing, and failure tests.
5. **Snapshot consistency:** The engine's safe point-in-time capture mechanism must be
   confirmed. Raw file copying is not assumed safe.
6. **Timing guarantees:** Election bounds depend on scheduler, network, and durable-write
   latency. The specification's bound must be reconciled with deployment guidance and
   tested under load.
7. **Two-node expectations:** A two-voter deployment cannot provide write availability
   after one failure. Documentation and configuration validation must prevent misleading
   expectations.
8. **Membership recovery:** Forced reconfiguration after quorum loss can discard data or
   create split brain. Its interface, authorization, and cluster-identity rotation need
   independent safety review.
9. **Rolling compatibility:** Command and metadata version negotiation needs a concrete
   supported matrix before rolling upgrades can be represented as supported.
10. **Proxy semantics:** Forwarding writes through followers may complicate authentication,
    cancellation, body limits, tracing, and timeout ambiguity. Returning not-leader may
    be safer for the first implementation.
11. **Retention determinism:** Expiration based on leader-selected timestamps must remain
    compatible with existing retention semantics and avoid mass expiration after a long
    outage.
12. **Resource exhaustion:** Slow peers and snapshots can retain logs and consume disk.
    Backpressure, quotas, and safe eviction policy require concrete limits.
13. **Observability privacy:** Topology and replication metadata are operationally useful
    but may be sensitive. Access control for dashboard and metrics needs deployment-level
    definition.
14. **Platform durability:** Filesystem rename and sync semantics vary. Supported platforms
    need explicit crash-consistency tests rather than assumptions.
15. **Formal assurance:** Fault tests provide evidence but cannot enumerate every protocol
    interleaving. A protocol model or model-checking effort should validate election,
    commitment, membership, and snapshot invariants.

These are design gaps and decisions for independent review and successor engineering.
They are not represented as already satisfied.

## Review boundary

Automated structural checks can establish that this artifact exists, names the governing
specification, contains required sections, and has a contiguous requirement matrix. They
cannot establish semantic fidelity, protocol safety, feasibility against concrete source
locks and persistence boundaries, or runtime availability. Human reviewers must check
the requirement mappings against this document's own sections, inspect the current
implementation, and reject references to nonexistent behavior.

This proposal intentionally leaves implementation, executable HA evidence, operational
qualification, and milestone closure to later gates. No tag or publication action is
part of this document.

### TRACE-020 mechanism grounding

TRACE-020 requires the proposed HA work to remain grounded in the actual v0.9.0 write and persistence mechanisms rather than introducing an abstract replication layer disconnected from Traza. The following boundaries are current-state observations plus proposed integration rules; they are not claims that HA behavior exists today.

- **WriteBuffer:** In the carried-forward engine, accepted records are accumulated through the existing `WriteBuffer` path before durable segment publication. The proposed leader must assign a monotonically increasing log position and term to a batch before that batch can become externally acknowledged. Followers must apply the same ordered batch through an equivalent buffer-to-segment transition, while treating retries for an already applied `(term, position)` as idempotent. A crash before quorum acknowledgement leaves the batch uncommitted and therefore ineligible for recovery as acknowledged data; a crash after quorum acknowledgement requires the new leader to retain or reconstruct that committed prefix before serving writes.
- **SegmentWriter:** `SegmentWriter` is the concrete segment-encoding and publication boundary, not a substitute consensus log. Replication must carry canonical logical records and their ordering metadata, then use `SegmentWriter` locally so each replica preserves the sanctioned segment format and validation rules. A partially written or unfinalized segment must never advance the replica's durable applied position. On restart, the replica validates published segments, discards or quarantines incomplete output according to the existing recovery rules, and resumes from the last durably recorded applied position; checksum, framing, or publication failure makes that replica unavailable for quorum progress until repaired or caught up.
- **primary-key index:** The in-memory primary-key index is derived acceleration state over the durable segment history. It must be updated only in the same ordered apply step that makes a committed record visible, and it must never be replicated as independent authority. Startup, snapshot installation, or catch-up rebuilds the primary-key index from the validated committed segment set before the replica may answer reads at the advertised applied position. If index reconstruction fails, if an index points beyond the durable committed prefix, or if duplicate application would change the winning record, the node must fail closed, rebuild, and remain outside read and leadership eligibility until the invariant is restored.
- **supersede journal:** The existing supersede journal records the old-to-new segment transition used by segment replacement. Under the proposed HA protocol, compaction and supersession are deterministic local maintenance over an already committed logical prefix; journal files themselves are not consensus decisions and must not cause a log position to be acknowledged. Recovery must finish or roll back an interrupted journaled transition using the existing atomicity rules before exposing segments or rebuilding indexes. Replicas may compact at different times, but both the pre-supersede and post-supersede layouts must represent the same committed records. Missing operands, conflicting journal state, or validation failure fences the replica from reads and leadership rather than guessing which physical layout is authoritative.

Together these rules define one authority boundary: consensus metadata decides which logical record prefix is committed; `WriteBuffer` stages ordered application; `SegmentWriter` creates validated durable segments; the supersede journal makes physical replacement recoverable; and the primary-key index is rebuilt derived state. Promotion is permitted only after all four relevant recovery checks establish that the candidate exposes exactly its durable committed prefix. The implementation and failure-injection tests needed to establish these properties remain successor work and are an explicit gap in this design-only artifact.

