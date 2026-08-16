# Module map

What each file in `src/` is responsible for, and where to start looking for a
given change.

## Crate rules

Two direct dependencies, `serde` and `serde_json`. HTTP, threading, hashing,
protobuf decoding, SHA-256, and file I/O are all standard library or
hand-written. The crate is `#![forbid(unsafe_code)]` and `missing_docs` is
denied, so every public item carries a doc comment and clippy enforces it.

## The engine

### [`src/lib.rs`](../../src/lib.rs)

The datastore. `Store`, `Span`, `Event`, `Link`, `SpanFilter`, `SpanCursor`,
`Config`, `CompactionConfig`, `Durability`, `Stats`, `Error`.

It owns:

- the write buffer and its primary-key upsert;
- the ingest path (`ingest`, `ingest_batch`, `admit`) and the acknowledgement
  ordering;
- flushing and segment writing (`flush_locked`, `write_segment`);
- every read path (`get_trace`, `query`, `query_after`, `stats`), the pinned
  `SnapshotView` a multi-page read pages through, and the resolution both share
  (`query_view`, `attribute_union_view`);
- compaction (`compact_segments`, `merge_tail_run`), TTL expiry
  (`compact_expired`, `expire_before`), the maintenance lock that serializes
  them, and the supersede journal;
- the single-writer directory lock, including stale-lock reclamation;
- span↔record conversion and the reserved index keys.

By far the largest file, and the one where the [invariants](invariants.md)
live. Most engine changes start here.

### [`src/segment.rs`](../../src/segment.rs)

The on-disk segment format: encoding, parsing, and file-backed reading. An
opened segment owns its file handle and decoded index maps only — records are
decoded when a query selects their offsets, and no decoded record vector is
retained. Byte layout: [segment format](../segment-format.md).

### [`src/generation.rs`](../../src/generation.rs)

Generations: the one state every recovery domain agrees on. Owns the manifest
(`state-manifest.json` — every load-bearing file with its length and SHA-256,
plus the log position it folded through), `CURRENT` and its staged-rename
publication, the digest walk that builds a manifest, verification against one,
the staged install behind `Store::restore`, and the sweep that retires
superseded manifests.

The engine's files never move: a generation *references* the working set in
place, because segment paths are load-bearing (invariants 1 and 5). What
changes hands is the manifest. A pin is a hard-link farm of one manifest's
files, which is why a backup copy survives compaction unlinking the originals
underneath it.

`Store::checkpoint`, `pin_generation`, `verify_generation` and `restore` in
[`src/lib.rs`](../../src/lib.rs) are the operations built on this; invariant 12
states the ordering rules they must not break.

### [`src/wal.rs`](../../src/wal.rs)

The write-ahead log with group commit. The file opens with the 8-byte magic
`TRZWAL02`; framing is `[u32 length][u32 crc32][u64 epoch][u64 sequence][payload]`,
the payload being one batch's JSON. The `(epoch, sequence)` stamp names the
generation a frame was appended under, and recovery replays a frame only when
that stamp is strictly after the live generation's `folded_through` — see
[`src/generation.rs`](#srcgenerationrs) and invariant 12.
Recovery streams frames one at a time and distinguishes a torn tail (bytes the
frame declared are missing — dropped, and the file truncated back to the last
good frame) from interior damage (a complete frame that fails its checksum or
decode — refuses to open, because frames after it may be acknowledged batches).
`rewrite` replaces the log with a given set of spans, which is how retention
deletes from the recovery authority rather than only from memory. The fsync
runs outside the state lock so concurrent batches coalesce into one sync; a
waiter wakes when some sync has covered its LSN.

Also the authoritative statement of the **macOS `fsync` caveat** — see
[durability](../operations/durability.md).

### [`src/payload.rs`](../../src/payload.rs)

Content-addressed offloading of oversized string attribute values to
`payloads/<aa>/<sha256>.bin`, replaced in the span by a `$payload` reference.
Includes a dependency-free SHA-256 (FIPS 180-4) verified against the standard
test vectors.

### [`src/annotations.rs`](../../src/annotations.rs)

Post-hoc annotations: an append-only JSONL log fsynced per append with an
in-memory index by trace. A torn trailing line is ignored, matching the segment
layer's crash-consistency stance.

### [`src/evals.rs`](../../src/evals.rs)

The eval entity model: datasets, versions, examples, experiments, runs —
identity and addressing only, deliberately without a runner, a scorer library,
or a UI. One append-only JSONL log (`evals.jsonl` at the store root), fsynced
per mutation with the annotation log's torn-tail healing, wholly resident in
memory (datasets are curated artifacts, not span-scale), and rewritten only
inside the erasure barrier when a tenant subject purges everything a tenant
owns — id floors survive the rewrite so erased dataset and experiment ids are
never reissued. Example bodies and version manifests are content-addressed by
SHA-256 over the module's own `canonical_json`, whose byte form is pinned by
test: these digests are persisted identity and must not depend on a
dependency's map-ordering feature flag. Also home to the score aggregation
(`summarize_scores`, `diff_scores`) behind the experiment summary and diff
endpoints.

### [`src/analytics.rs`](../../src/analytics.rs)

Sessions and LLM token/cost aggregation — derived views over ordinary spans, no
new record type. Holds the per-segment rollup cache (segments are immutable, so
a rollup is computed at most once per process and keyed by path) and the
supersede prefilter that makes a cached rollup safe to reuse.

### [`src/semconv.rs`](../../src/semconv.rs)

Semantic-convention normalization: folds the OpenLLMetry / OTel GenAI
vocabulary and Traza's native `llm.*` shorthand into one set of facts. A pure
function over a span's attribute map. **This is the single source of truth for
key precedence** — [`docs/llm-semantics.md`](../llm-semantics.md) documents it
and `ui/src/lib/spans.js` mirrors it, so a change here needs all three to move
together.

### [`src/expiration.rs`](../../src/expiration.rs)

A four-line placeholder. Expiration is implemented in `Store::expire_before` in
`lib.rs`; this module reserves the unit and contains no public API.

## Ingest surfaces

### [`src/otlp.rs`](../../src/otlp.rs)

OTLP/HTTP **JSON** ingest mapping: `ExportTraceServiceRequest` to `Span`. Hex
ids lowercased, `*TimeUnixNano` accepted as string or number, typed `AnyValue`
attributes flattened, resource `service.name` becoming the service, scope
attributes merged beneath span attributes.

### [`src/otlp_pb.rs`](../../src/otlp_pb.rs)

OTLP/HTTP **binary protobuf**, hand-rolled and dependency-free. Rather than
duplicating the mapping, it lowers protobuf into exactly the JSON `Value` shape
`otlp.rs` expects and hands off — one mapping, two encodings, so every JSON
conformance behaviour applies to protobuf automatically. All slicing is
bounds-checked; malformed input yields a decode error, never a panic.

## Server-side support

### [`src/auth.rs`](../../src/auth.rs)

Bearer authentication: `TRAZA_TOKENS` parsing, `ro`/`rw` scopes, constant-time
token comparison, and the 401/403 verdicts. `AuthConfig` implements a redacted
`Debug` so an accidental structured log cannot disclose credentials, and parse
errors name the defect without echoing the value.

### [`src/mcp.rs`](../../src/mcp.rs)

The Model Context Protocol surface: JSON-RPC framing, `initialize` and version
negotiation, the ten tool schemas and handlers, five resources and three URI
templates, four prompts, and the renderers that turn spans into something a
context window can hold. Tool handlers call `Store` directly — never back
through HTTP — so the module works with no listener in the process at all,
which is what `tests/mcp.rs` relies on.

Two things here are load-bearing rather than cosmetic. `sanitize` is the only
path stored text may take into a result: it escapes control characters and
neutralizes the telemetry delimiter, so a span value cannot forge a row or
close the untrusted block early. And `clamp_report` is the last word on size —
no result may exceed the configured ceiling, and no truncation may be silent.
Documented in [the MCP guide](../guide/mcp.md).

### [`src/ui.rs`](../../src/ui.rs)

Static serving of the built dashboard from a directory on disk: the discovery
order for `--ui-dir`, the shell routes (`/`, `/dashboard`, `/dashboard/`),
content types by extension, and path-traversal refusal against the
canonicalized root.

### [`src/metrics.rs`](../../src/metrics.rs)

The instrumentation primitives (`Counter`, `Latency`) and the engine's
`Metrics` set, plus Prometheus rendering. Latencies land in power-of-two
nanosecond buckets, which is why percentiles are approximate — read the module
docs before publishing a number from here. See
[monitoring](../operations/monitoring.md).

## Test and demo data

### [`src/seed.rs`](../../src/seed.rs)

Deterministic synthetic telemetry: the corpus `tests/scenarios.rs` asserts
against and the `seed` binary loads. Aims at coverage of real shapes rather
than volume — agentic tool-calling traces, all three attribute dialects,
multi-turn sessions, multimodal messages, RAG, streaming, failures with linked
retries, parallel fan-out, oversized payloads, and ordinary non-LLM traffic.
The same options always produce the same corpus byte for byte.

### [`src/media.rs`](../../src/media.rs)

Synthesized demo media for that corpus — real PNG, GIF, WAV, and SVG bytes
encoded from scratch with no image or audio crate, because the dashboard's
media rendering only means anything if the bytes actually decode.

## Binaries

### [`src/bin/traza-server.rs`](../../src/bin/traza-server.rs)

The HTTP/1.1 server: argument parsing, the connection pool and its bound,
request framing, the auth gate, route dispatch, chunked export streaming, and
server-side metrics. **The authoritative source for routes and query
parameters** — the API reference is checked against this file, not the other
way round.

### [`src/bin/bench.rs`](../../src/bin/bench.rs)

The canonical end-to-end benchmark. Builds and starts the release server, drives
it over HTTP, and rewrites `canonical-corpus.md` from its own
measurements.

### [`src/bin/ingest-bench.rs`](../../src/bin/ingest-bench.rs)

The ingest matrix over protocol, keep-alive, concurrency, and durability.
Reports the median of N runs with its spread and refuses to report a rate from
a run that shed a connection or stored fewer spans than it acknowledged.

### [`src/bin/seed.rs`](../../src/bin/seed.rs)

Loads the seed corpus, either directly through the engine (`--data-dir`) or
over HTTP into a running server (`--url`). Two modes because a data directory
has exactly one writer.

## Elsewhere in the tree

| Path | What it is |
|---|---|
| [`tests/`](../../tests) | Integration tests — see [testing](testing.md) |
| [`ui/`](../../ui) | The React dashboard; `npm run build` emits `ui/dist` |
| [`ci.sh`](../../ci.sh) | The merge gate |
| [`docs/`](../README.md) | This documentation set |
