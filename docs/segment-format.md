# Segment Format v2: indexed segments

The shipped layout is described first; the original v0.3 design proposal is
retained below it as history. **The proposal does not describe the format that
shipped** — it predates the implementation and differs in the layout, the
section list, and the compatibility story. Read the first section for the
format; read the second only for the reasoning.

## Shipped layout

Verified against `src/segment.rs` (`Header::parse_with_total` and `encode`) and
pinned by `tests/segment_format_acceptance.rs`.

A segment is one file named `segment-<20-digit id>.seg`, written temp + fsync +
atomic rename. It is a fixed 80-byte header followed by four contiguous
sections, in this order, with no gaps and nothing trailing:

```
[ 80-byte header ]
[ records        ]  encoded records, ascending timestamp order
[ record offsets ]  u64 per record, relative to the record region
[ trace index    ]  trace id -> record offsets
[ attribute index]  (key, value) -> record offsets; runs to EOF
```

Header fields, little-endian, at fixed byte offsets:

| Offset | Size | Field |
|---:|---:|---|
| 0 | 8 | magic, `TRAZASEG` |
| 8 | 2 | format version (`2`) |
| 10 | 2 | header length (`80`) |
| 12 | 4 | reserved, zero |
| 16 | 8 | record count |
| 24 | 8 | records offset |
| 32 | 8 | records length |
| 40 | 8 | record-offset index offset |
| 48 | 8 | record-offset index length |
| 56 | 8 | trace index offset |
| 64 | 8 | trace index length |
| 72 | 8 | attribute index offset |

The attribute index has no stored length: it runs from its offset to EOF, so
its length is derived from the file size. The reader rejects a segment whose
sections are not contiguous from the end of the header, whose record-offset
index is not exactly `record_count * 8` bytes, or whose sections exceed the
file.

Each record is: timestamp `u64`, trace-id length `u32`, attribute count `u32`,
payload length `u32`, reserved `u32`, then the trace id, then that many
length-prefixed key/value pairs, then the opaque payload.

`Segment::open` reads only the header and the three index sections into memory;
record payloads stay on disk and are read by exact byte range on demand. That
is what makes stores larger than RAM serveable.

### Compatibility

v1 JSONL segments are **not** readable. `Store::open` refuses a directory
containing a `.jsonl` segment with an error pointing at traza 0.3.x for
migration — failing loudly beats silently hiding persisted data.

---

# Original design proposal (v0.3, historical)

Everything below is the proposal as written before implementation. It is kept
for the reasoning behind the design, not as a description of the format. Where
it disagrees with the section above — notably the trailing-footer arrangement,
the JSONL payload, and readable v1 segments — the section above is correct.

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
