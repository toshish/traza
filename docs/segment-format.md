# Segment Format: indexed segments

The shipped layout is described first; the original v0.3 design proposal is
retained below it as history. **The proposal does not describe the format that
shipped** — it predates the implementation and differs in the layout, the
section list, and the compatibility story. Read the first section for the
format; read the second only for the reasoning.

## Shipped layout

Verified against `src/segment.rs` (`Header::parse_with_total` and `encode`) and
pinned by `tests/segment_format_acceptance.rs`.

A segment is one file named `segment-<20-digit id>.seg`, written temp + fsync +
atomic rename. It is a fixed 104-byte header followed by five contiguous
sections, in this order, with no gaps and nothing trailing:

```
[ 104-byte header ]
[ records         ]  encoded records, ascending timestamp order
[ record offsets  ]  u64 per record, relative to the record region
[ trace index     ]  trace id -> record offsets
[ attribute index ]  (key id, value digest) -> record offsets
[ content index   ]  word filters per 128-record block; runs to EOF
```

Header fields, little-endian, at fixed byte offsets:

| Offset | Size | Field |
|---:|---:|---|
| 0 | 8 | magic, `TRAZASEG` |
| 8 | 2 | format version (`6`) |
| 10 | 2 | header length (`104`) |
| 12 | 4 | reserved, zero |
| 16 | 8 | record count |
| 24 | 8 | records offset |
| 32 | 8 | records length |
| 40 | 8 | record-offset index offset |
| 48 | 8 | record-offset index length |
| 56 | 8 | trace index offset |
| 64 | 8 | trace index length |
| 72 | 8 | attribute index offset |
| 80 | 8 | minimum record timestamp |
| 88 | 8 | maximum record timestamp |
| 96 | 8 | content index offset |

Neither the attribute index nor the content index stores its own length. The
attribute index is bounded by where the content index begins; the content
index runs to EOF. The reader rejects a segment whose
sections are not contiguous from the end of the header, whose record-offset
index is not exactly `record_count * 8` bytes, or whose sections exceed the
file.

Each record is: timestamp `u64`, trace-id length `u32`, attribute count `u32`,
payload length `u32`, reserved `u32`, then the trace id, then that many
length-prefixed key/value pairs, then the opaque payload. **Records carry
attribute value text; the index does not.**

### The attribute index

```
u32                     key count
  u32 + bytes           each attribute key name, length-prefixed
u32                     entry count
  u32                   key id (index into the dictionary above)
  16 bytes              digest of (key, value), see src/hash.rs
  u32                   posting count
  u64 × posting count   record offsets, ascending
```

Entries are written in `(key id, digest)` order, and the dictionary in sorted
key order, so encoding the same records twice produces identical bytes.

Keys are interned because they are a schema — tens of them, repeated on every
span, and an operator needs their names to read a cost report. Values are
replaced by a digest because they are data: unbounded in size, and for LLM
traffic they *are* the corpus. Through v3 the index stored value text, and a
store of prompts therefore held every prompt in RAM for the life of the
segment. See [capacity](operations/capacity.md#memory) for what that cost.

**A digest probe returns candidates, not matches.** Every caller must check a
decoded record against the filter before returning it;
`Segment::attribute_candidate_offsets` is named to make that unmissable, and
`tests/segment_format_acceptance.rs` forges a collision to prove the check is
load-bearing. A 128-bit digest will not collide naturally, which is exactly
why the safety argument cannot rest on a passing query test.

### The content index

Answers "which spans mention this word" without storing the words.

```
u32   reserved, zero
u32   records per block (128)
u32   block count (0 means: no content index, see below)
u32   hashes per token (4)
u64   summary filter size in bits
u64   block filter size in bits
[ summary filter    ]  one Bloom over every token in the segment
[ bit-sliced blocks ]  one ROW per bit position, one BIT per block
```

Records are grouped into blocks of 128. Each block has a Bloom filter over the
distinct tokens of its records' text, and those filters are stored
**transposed**: row *p* holds bit *p* of every block's filter.

That transposition is the whole design. Stored per block, testing one word
against a segment means reading every block's entire bitmap — hundreds of
kilobytes to look at a handful of bits. Stored as rows, the same test is one
read of `block_count` bits, so a two-word query reads tens of bytes per
segment. Only the summary filter is held resident, capped at 32 KiB, which is
why the content index's memory cost scales with segment count and not with how
much text a store holds.

Tokens are maximal runs of ASCII alphanumerics, lowercased, truncated at 40
bytes; the tokenizer and the bit derivation are format constants pinned by
test in `src/content.rs`.

**A block filter admits candidates, never answers.** Bloom filters have false
positives, and a block is 128 records wide, so most admitted records will not
match; `content::Query::matches` against the decoded span decides. This also
fixes the feature's semantics: content search is WORD search, because a word
index cannot soundly over-approximate a substring search. Searching `refund`
against a span reading `refunds were issued` would be skipped by the filter
while a substring match would have returned it — a wrong answer, not a slow
one. See `src/content.rs` for the full argument.

**`block_count = 0` means the index is absent, not empty.** A segment whose
records carry no indexable text, or one written with `--no-content-index`, must
be SCANNED by a content query. Reading absence as "holds nothing" would make
content search silently return no rows. Note that the section is always
present: "indexed nothing" is stated inside it rather than by its absence,
which is what lets the header field bound the attribute index unconditionally.

`Segment::open` reads only the header, the three index sections, and the
content index's prologue and summary filter into memory; record payloads and
the bit-sliced block rows stay on disk and are read by exact byte range on
demand. That is what makes stores larger than RAM serveable.

### Versioning

**There is exactly one readable version.** A file declaring any other is
refused, including a JSONL segment from 0.3.x, which carries no magic at all.

It was not always one. The format grew by appending header fields behind
`if version >= N` gates, and each gate turned a field into an `Option` that
every reader downstream had to treat as "unknown, therefore assume the worst":
a segment whose timestamp range could not be read had to be scanned by every
time-bounded query, and a second attribute-index decoder existed solely to read
the encoding that predated digests. Those branches were removed and the header
fields became plain values, so the pruning path no longer carries a case where
it cannot prune.

**Versions 1 through 5 are spent.** 1 was JSONL. 2 was written by v0.16 and
v0.17, 3 by v0.18 and v0.19. **4 and 5 were never released** — they existed only
on unreleased `main`, so no tag writes them and no tag reads them. None of the
five opens now. The README's pre-1.0 terms permit an on-disk break between 0.x
versions, and this is one: `Store::open` refuses such a segment and names it,
never advising deletion.

### Migrating between formats

**Take a backup by one of the two procedures in the
[durability guide](operations/durability.md#backup):** stop the server and copy
the directory, or take a filesystem snapshot that is atomic across the whole
directory. Copying a live directory file by file is *not* safe — an in-flight
flush can change the segment set between files — and that guide is the single
source of truth for it. Then read the copy with the build that wrote it.

A span export is a reasonable way to move a **dataset**, but it is not a
migration of the store:

| Part of the store | In `GET /v1/export`? |
|---|---|
| Spans | **yes** — every one, as of the instant the export began. It pins a snapshot, and that snapshot copies the write buffer, so spans not yet sealed into a segment are included |
| Offloaded attribute values | **no** — left as `{"$payload": "sha256/…"}` references; the bytes stay in `payloads/` |
| Annotations | **no** — a separate surface (`/v1/annotations`) the export does not touch |

The design document gives the underlying reason: a span export "cannot pin
[annotations and payload bytes] at all" — there is no consistent point across
the store's independent recovery domains for it to pin
([generations-design.md](generations-design.md)). Closing that is what the
generation/checkpoint boundary is for, and it is scheduled before 1.0.

So export-and-reingest is a complete migration only for a store with no
offloaded payloads and no annotations. Otherwise: back up, and read the backup
with the build that wrote it.

### The policy from here

This cut is a one-time exception, taken while the store holds no data anyone
depends on. **"Every layout change makes all prior files unreadable" is not a
policy** — it treats stored telemetry as disposable, and a datastore does not
get to do that.

The rule from v6 onward is three planes, kept apart on purpose:

1. **Runtime reads exactly one canonical format.** No `if version >= N`, no
   `Option` field standing in for "unknown", no compatibility branch in the
   query path. That is what this change bought, and it is the part to preserve.
2. **Version numbers stay monotonic.** An identifier written by a release is
   never reused for a different layout.
3. **A format bump ships with a migrator** — an explicit, resumable conversion
   from the previous format into the current one, run offline or at startup,
   never woven into the read path. Reading an old format is code that has to
   exist somewhere; the win is that it lives there rather than in every query.

Point 3 is the part this change does not pay for, and that is worth stating
rather than hiding: a migrator from v2/v3/v5 would mean resurrecting precisely
the decoders just deleted, to serve stores that do not exist. The debt was
declined once, on the last occasion it could be declined cheaply. v6 is where
it starts being paid.

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
