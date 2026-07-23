# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.7.0] - 2026-07-23

### Added
- **Roadmap leg 3 — LLM-observability semantics.** Documented gen-AI span
  conventions ([docs/llm-semantics.md](docs/llm-semantics.md)): `llm.*`
  span names, model/token/temperature/stop-reason/tool/cost attributes
  (index-served like any attribute), prompt and completion payloads as
  span events so large text never enters the filter index, and four
  concrete query recipes over the existing API. Process-level tests prove
  every recipe through both `/v1/spans` and OTLP ingest. Purely additive:
  one doc, one test target, one README section.

## [0.6.0] - 2026-07-23

### Added
- **Roadmap leg 2 — OTLP/HTTP JSON ingest.** `POST /v1/traces` accepts an
  OpenTelemetry ExportTraceServiceRequest in OTLP/HTTP JSON and maps it
  onto the span model: hex ids lowercased, `*TimeUnixNano` accepted as
  string or number, typed `AnyValue` attributes (string/int/double/bool/
  array/kvlist) flattened to plain JSON, resource `service.name` becoming
  the span's service (`unknown_service` fallback), scope attributes
  merging beneath span attributes, events mapped, and OTLP status codes
  becoming `ok`/`error`/empty. Structurally invalid requests 400 with a
  diagnostic; the existing `/v1/spans` contract is untouched. No new
  dependencies. Conformance-tested end to end against the real binary,
  including index-served queries over OTLP-ingested spans.

## [0.5.0] - 2026-07-23

### Changed
- **Roadmap leg 1 — larger-than-RAM reads.** Segments are file-backed:
  `Segment::open` reads only the header and index sections into memory and
  serves every record access by reading exactly the needed byte range from
  the file (std `Seek`+`Read`; no mmap, no new dependencies). Flushing
  reopens the new segment file-backed, so no resident payload copy survives
  the write either. `Store::resident_payload_bytes()` exposes the invariant
  (zero after open and after flush). Measured at a 10M-span corpus
  (2.4 GB on disk): **0.71 GB peak server RSS** (was ~2.4 GB
  bytes-resident, ~5 GB in the pre-v2 engine); trace lookup p50 0.8 ms,
  attribute filter p50 8.7 ms — RAM is O(indexes) and stores larger than
  memory serve correctly.
- The lazy limited-query merge caches per-source head timestamps: with
  file-backed segments every peek is a disk read, and re-peeking all
  sources per pop had regressed the 10M filter to 125 ms; cached heads
  restore 9.6 ms.

### Known deviation
- The leg's relative bound ("1M latencies within 2x of 0.4.0") is missed
  on filter p95: 3.34 ms vs 1.27 ms (2.6x) — the cost of on-demand file
  reads at sub-5 ms magnitudes. The absolute gate (< 300 ms) passes with
  ~90x headroom; recorded here rather than tuned away.

## [0.4.0] - 2026-07-23

### Changed (breaking)
- **Span identity is a primary key.** (trace_id, span_id) is enforced
  unique: re-ingesting an existing pair replaces the stored span — in the
  write buffer, across flushes, and across restart. Last write wins on
  every read path (trace, filtered, and limited lazy queries), so client
  retries are idempotent and never produce duplicate copies. This reverses
  0.3.1's at-least-once visible-duplicate semantics.
- **v1 JSONL segments are no longer read.** The engine is v2-only; opening
  a directory containing a legacy `.jsonl` segment fails loudly with a
  migration pointer (read with 0.3.x first). The dual-format code path is
  removed.

## [0.3.1] - 2026-07-23

### Fixed
- **Data loss across restart**: next-segment numbering only recognized
  `.jsonl` names, so a reopened v2-only store restarted at id zero and the
  next flush renamed over an existing segment, destroying persisted spans.
  Both suffixes count now, and `write_segment` refuses to replace an
  existing file outright.
- **Acknowledged duplicate cardinality survives restart**: content-based
  duplicate healing is gone. Compaction rewrites are journaled with a
  supersede marker written before the rewrite begins; recovery finishes an
  interrupted rewrite from the journal in either direction and never
  deduplicates by content, so legitimately re-ingested identical spans keep
  both acknowledged copies.
- A corrupt v2 header with an out-of-range attribute-index offset returns
  `Error::Corrupt` instead of panicking through unsigned subtraction.
- User attributes named with a NUL prefix (for example `"\u{0}service"`)
  can no longer overwrite the reserved service/name index keys and poison
  those queries; such attributes are stored verbatim but excluded from the
  index, and filters on them decline index use symmetrically.

### Changed
- **Limited queries are lazy end to end**: per-segment index postings stay
  undecoded and a k-way merge pops candidates in start-time order, decoding
  and re-verifying only what the limit returns. Measured: attribute filter
  p50 18 ms -> 0.53 ms at 1M spans and 209 ms -> 2.9 ms at 10M — the 10M
  advisory target (<100 ms) is closed with 35x headroom.
- README limitations and roadmap reflect the v2 engine (byte residency,
  journaled compaction, remaining mmap/streaming work).

## [0.3.0] - 2026-07-23

### Changed
- **Segment format v2**: new segments are indexed binary files (`.seg`) —
  JSON span payloads with an embedded record-offset index, trace index, and
  attribute index, written with the same temp + fsync + atomic-rename
  discipline. v1 JSONL segments remain fully readable beside v2 and heal
  through the same duplicate-recovery path; TTL rewrites produce v2.
- **Byte-resident reads**: opening a store no longer materializes spans.
  v2 segments hold raw bytes plus their indexes; spans parse on demand,
  only for records a query returns. `Store::resident_persisted_span_structs`
  exposes the invariant (zero after a v2-only open).
- **Index-served queries**: `get_trace` binary-searches the trace index;
  filters narrow through service/name/attribute indexes or time range and
  re-verify every predicate on the parsed span. Measured on the bundled
  benchmark: trace lookup p50 0.185 ms at 1M spans (was 14.2 ms) and
  0.536 ms at 10M (was 145.6 ms); attribute filter p50 18 ms at 1M
  (was 66.5 ms) and 209 ms at 10M (was 4,395 ms).

### Known limits
- The 10M advisory filter target (<100 ms) is not yet met: candidate
  payload parsing dominates large result groups. Posting-list
  intersection and parse-avoidance are the next optimization.
- Segment bytes are read into memory at open (no mmap yet); resident cost
  is file bytes + indexes rather than parsed structs.

## [0.2.3] - 2026-07-22

### Fixed
- Segment writes are buffered. `serde_json::to_writer` against a raw `File`
  issued one write() syscall per JSON token, making flush cost ~140 us per
  span and capping measured end-to-end ingest at 5,450 spans/s. A 256 KiB
  `BufWriter` restores flush to ~1.5 us per span; the regenerated benchmark
  measures 138,180 spans/s and the ingest gate passes again.

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
