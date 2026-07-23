# Leg 1: larger-than-RAM reads

## Problem

A v2 segment currently holds its entire file as a resident `Vec<u8>`.
Indexes are a few percent of that; the payload region is the rest. At 10M
spans that is ~2.4 GB resident for data the queries mostly never touch.

## Design

`segment_v2::Segment` gains a backing mode: instead of `bytes: Vec<u8>`, it
holds the opened `File` plus the parsed indexes (header, record offsets,
trace index, attribute index — read once at open). Every record access
(`record_at_offset`, `timestamp_at`, `record`, `query_*`) reads exactly the
byte range it needs via `Seek`+`Read` (pread-style; std-only, no mmap and no
new dependencies). The OS page cache provides locality; the engine provides
correctness.

Concurrency: `File` reads require `&mut` or `try_clone`; use a per-call
`try_clone()` or an internal `Mutex<File>` — measure both, keep the simpler
one that holds the bench gates. Writes are unaffected (segments are
immutable after rename).

## Acceptance (blocking, executable oracles)

1. `./ci.sh` green; all existing tests unmodified and passing.
2. New test: after open of a flushed store, process-measurable resident
   segment payload is zero — expose `Store::resident_payload_bytes()`
   (sum of resident payload buffers; must be 0) alongside the existing
   `resident_persisted_span_structs`.
3. New test: a store larger than the configured resident budget still opens
   and serves correct trace + filtered queries (construct with a small
   corpus; the property is structural, not scale).
4. Canonical 1M bench: all three gates PASS; trace p95 and filter p95
   within 2x of the 0.4.0 numbers recorded in BENCHMARKS.md.

## Non-goals

mmap, compression, streaming HTTP responses (chunked transfer), manifest.
