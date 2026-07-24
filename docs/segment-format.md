# Segment Format v2: indexed segments

> Historical design note from the v0.3 transition. The current engine is
> v2-only and file-backed: `Segment::open` reads the header and index sections,
> then reads exact payload ranges on demand. Legacy v1 JSONL segments fail
> startup with a migration pointer. Current behavior and guarantees are
> documented in the README and `src/segment.rs`; proposal language below is
> retained as design history.

## Problem (measured)

The engine keeps every span materialized in memory (`Segment { spans: Vec<Span> }`,
~450 B and ~10 heap allocations per span) and answers every query by walking all
of them. Measured on the bundled benchmark:

| Metric | 1M spans | 10M spans |
|---|---:|---:|
| Trace lookup p50 | 14.2 ms | 145.6 ms |
| Attribute filter p50 | 66.5 ms | 4,395 ms |
| Server RSS | ~0.5 GB | ~5 GB |

Two independent causes: no index (every query touches all N spans) and a
pointer-chasing row layout (per-span visit cost rises ~7x once the corpus
outgrows the CPU caches). Both are fixed by this format.

## Design

A v2 segment is ONE file, written with the existing temp + fsync + atomic
rename discipline, laid out as:

```
[ JSONL span payload, byte-identical to v1 ]
[ index block ]
[ fixed-size footer: index offset, index length, span count,
  min_start_time_ns, max_start_time_ns, format version, magic ]
```

The index block contains, all offsets relative to payload start:

1. **Trace index**: sorted (trace_id, line byte offset, line length) entries —
   binary-searchable; a trace's spans are found without touching other lines.
2. **Posting lists**: for `service`, `name`, and each attribute key/value pair:
   the sorted list of line offsets whose span matches. Dictionary of strings
   once per segment.
3. **Time bounds** are in the footer for whole-segment skipping.

Encoding: length-prefixed strings and little-endian u64s — no new
dependencies (serde_json for the payload lines only).

### Memory rule

Open no longer materializes spans. A loaded segment holds:
- the payload bytes (mmap if straightforward with std-only constraints,
  otherwise a `Vec<u8>` read once), and
- the parsed index (small: a few percent of payload).

`Span` structs are parsed on demand from payload lines, only for spans a
query actually returns. The write buffer (unflushed spans) is unchanged.

### Read paths

- `get_trace`: binary-search the trace index in every non-skipped segment,
  parse only that trace's lines. Plus the write buffer as today.
- `query`: intersect posting lists for indexed predicates (service, name,
  attributes) and time-skip segments via footer bounds; parse candidates and
  re-verify EVERY predicate against the parsed span (an index accelerates a
  filter, it never changes its semantics). Predicates with no usable index
  (e.g., min_duration alone) fall back to a full scan of the contiguous
  payload bytes — never to materialized structs.
- `stats`, `persisted_segment_spans`: derive from footers/payload parse.

### Compatibility

- v1 segments (no magic in the trailing footer position) remain readable via
  the legacy path: parse the JSONL fully at open, build the same index
  structures in memory, then treat identically. First compaction that
  rewrites a v1 segment writes v2.
- The lock discipline (writer before segments), crash recovery (orphan temp
  removal, duplicate-span healing at open), TTL semantics, and the wire
  contract are unchanged. Duplicate healing must work across v1/v2.

## Acceptance criteria

Blocking, each with an executable oracle:

1. `./ci.sh` green: fmt, clippy, full test suite.
2. All existing storage and server_on_engine tests pass unmodified (they pin
   crash recovery, lock discipline, healing, and the wire contract).
3. New tests prove: a trace lookup parses only the target trace's lines
   (instrumentable via a parse counter or by construction); v1 segments load
   and serve correctly; a v2 file survives kill-and-reopen; index-served
   results equal full-scan results on a randomized corpus (extend the
   existing naive-reference test to cover the indexed path).
4. Memory: after open of a flushed corpus, resident span structs are zero —
   verifiable by API (no `Vec<Span>` in `Segment`) and a stats/heap assertion.
5. `cargo run --release --bin bench` (1M canonical) regenerated: all three
   gates PASS with trace-by-id p95 under 5 ms.

Advisory (report, do not gate on machine-dependent numbers):

6. `TRAZA_BENCH_SPANS=10000000` run: trace lookup p50 < 1 ms, attribute
   filter p50 < 100 ms, server RSS under 1.5 GB. Record actuals in
   BENCHMARKS-10M.md style notes (do not overwrite BENCHMARKS.md).

## Non-goals

Columnar payload encoding, compression, OTLP, auth, replication — all stay
on the roadmap. The JSONL payload stays byte-identical to v1 precisely so
this change is index + residency only.
