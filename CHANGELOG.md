# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.2] - 2026-07-22

### Fixed
- `ttl_seconds: Some(0)` disables expiration as documented instead of
  expiring every existing span; the library `Config` TTL default is `None`
  as documented, no longer a silent seven days.
- Recovery heals crash-duplicated segments: exact-duplicate spans are
  dropped at open and fully-duplicate segment files deleted, closing the
  window where a crash mid-compaction returned two copies of surviving
  spans on reopen.
- An empty reclamation sentinel left by a reclaimer that died before
  recording its PID no longer wedges lock recovery: unreadable sentinels
  older than ten seconds are treated as corpses.
- The README introduction and features no longer claim a manifest,
  per-segment indexes, or log replay; the configuration tables match the
  code (server `--ttl-seconds` drives engine compaction; `--host` and
  `--flush-spans` documented).

### Changed
- BENCHMARKS.md regenerated against the engine-backed server. The ingest
  gate is honestly reported as MISSED (5,450 spans/s against the 50,000
  target); read gates pass. Closing the write-path gap is the top of the
  roadmap.

## [0.2.1] - 2026-07-22

### Fixed
- The documented ingest contract works again: timestamp aliases
  (`start_time_unix_nano`, `start_timestamp_ns`, `start_ns`, `start_time`
  and the matching `end_*` keys) are accepted, `parent_span_id`, `status`,
  `attributes`, and `events` are optional, and unknown span fields are
  stored and returned verbatim instead of silently discarded. This also
  un-breaks the bundled benchmark, which emits `start_ns`/`end_ns`.
- The documented search filters work again: `attr.KEY`, `min_duration_ms`,
  `since`/`until`, and the default `limit` of 100.
- `/v1/stats` exposes the documented `span_count`, `segment_count`, and
  `bytes_on_disk` keys alongside the engine's finer-grained fields.
- The server binds `0.0.0.0` again by default; `--host` overrides.
- TTL expiration no longer empties the in-memory segment set when a file
  operation fails mid-compaction; the store keeps serving its previous view
  and surfaces the error.
- Stale-lock reclamation is single-winner: a reclamation sentinel closes the
  window in which a slow reclaimer could delete a fresh lock and defeat the
  single-writer guarantee.
- The README architecture section describes the engine as built (JSON-lines
  segments, memory-resident, linear scans, no manifest or per-segment index
  yet) instead of the roadmap design, and names the compaction
  crash-atomicity bound in Limitations.

## [0.2.0] - 2026-07-22

### Changed
- `traza-server` is engine-backed: the segment engine is the server's only
  datastore. The HTTP wire contract is unchanged; the server-side append-only
  log, in-memory indexes, and startup replay are removed, and restart
  durability and crash recovery are the engine's.

### Added
- `POST /v1/flush` forces buffered spans into a durable segment on demand.
- `--flush-spans` server flag to tune the engine's flush threshold; `--port 0`
  binds an ephemeral port announced on stderr.
- Five process-level `server_on_engine` integration tests that drive the real
  server binary end to end, including an engine-authority cross-check and a
  kill-and-restart persistence test.
- Stale-lock reclamation: `Store::open` reclaims a lock file whose recorded
  owner process is verifiably dead, so a crashed server cannot permanently
  wedge its data directory. A live owner still rejects the open.

### Fixed
- Deadlock-capable lock-order inversion between `flush()` and `stats()`; all operations now follow a documented writer-before-segments discipline.
- `query()` and `get_trace()` now take an atomic combined snapshot of buffered and persisted spans, so a concurrent flush can no longer hide committed spans.
- Crash-orphaned segment temp files no longer wedge subsequent flushes: temp names are unique per process, and `Store::open` removes orphans during recovery.
- Concurrent writers are rejected: `Store::open` holds a lock file for the store's lifetime and a second open fails with `Error::AlreadyOpen`.

### Added
- Concurrency and failure-injection tests: deadlock detection, read-during-flush consistency (exactly-once), stale-temp recovery, and second-open rejection. Direct per-segment ordering assertion in the flush test.

- Renamed the project and crate to Traza (`traza`).

## [0.1.0] - 2026-07-20

### Added

- The tracing storage engine exposed as a Rust library.
- The `traza-server` HTTP server for the documented ingestion and query endpoints.
- The `bench` executable for measuring the existing datastore workloads.
- Four behavioral integration tests: buffer-flush persistence, crash recovery via reopen, randomized filter equivalence against an independent naive reference, and TTL compaction. (Persisted batch *ordering* is asserted only indirectly; a direct segment-order assertion arrives with the storage-correctness work.)
- Crate documentation, dual MIT/Apache-2.0 licensing, and release automation.

### Known Limitations

- This is an initial 0.1 release; consult README.md for the currently documented operational constraints and unsupported use cases.

[Unreleased]: https://github.com/toshish/traza/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/toshish/traza/releases/tag/v0.1.0
