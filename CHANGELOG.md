# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
