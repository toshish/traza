# Leg 6: high availability — design document (no implementation)

The final roadmap leg is deliberately re-scoped to a DESIGN DOCUMENT.
No replication code ships in this leg; the deliverable is
`docs/ha-design.md`, a decision-ready design grounded in traza's actual
architecture, plus the one-line README Roadmap update pointing at it.

## Scope — allowlisted files ONLY

`docs/ha-design.md` (new), `README.md` (the Roadmap HA line only),
this doc. Anything else is out of scope.

## The design document must

1. Ground every mechanism in the REAL engine, by name: the single-writer
   `Store` with its `WriteBuffer`, immutable v2 segments
   (`segment_v2`), the supersede journal used by TTL compaction, the
   `(trace_id, span_id)` primary key with last-write-wins, and the
   sentinel-based `DirectoryLock`. A design that could be pasted onto
   any datastore fails review.
2. Compare at least three architectures honestly — e.g. immutable
   segment shipping with a tailed buffer log, synchronous request
   mirroring, shared-storage failover — with the costs of each in
   durability, staleness, operational complexity, and code impact.
3. Recommend ONE, with: what ships between nodes and when; how the
   primary key makes replication idempotent under retry; consistency
   guarantees a reader on a replica actually gets (bounded staleness,
   monotonicity or lack of it); failover (detection, promotion,
   split-brain prevention building on the directory-lock/lease
   discipline); and client-visible behavior per endpoint during
   degraded states (ingest 503-and-retry vs reads served stale).
4. State non-goals explicitly (multi-writer, consensus quorums,
   cross-region latency budgets, automatic rebalancing).
5. End with an honest "Open questions" section — unknowns named, not
   papered over.

## Acceptance (blocking)

1. `docs/ha-design.md` exists with sections covering: architecture
   comparison, recommendation, replication protocol, consistency,
   failover, non-goals, open questions (structure grep oracle).
2. The doc names the real mechanisms (`WriteBuffer`, `segment_v2`,
   supersede journal, primary key, `DirectoryLock`) — a grep oracle
   proves the grounding.
3. README Roadmap HA line links to the design doc.
4. Diff-scope oracle: only allowlisted paths changed.
5. `./ci.sh` untouched and green (docs cannot break it; the oracle
   proves no code changed).

## Non-goals

Implementation, benchmarks, config flags, wire formats beyond sketch
level.
