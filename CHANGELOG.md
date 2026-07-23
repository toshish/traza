# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
