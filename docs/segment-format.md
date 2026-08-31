# Segment Format: indexed segments

This document has three parts. The [v7 format](#format-v7) is specified first
and is **authoritative for what is on disk**: it is the one version this
build writes and reads, and the format acceptance tests are written against
its tables. The [historical v6 layout](#format-v6-historical--the-migration-source)
follows — the format every release before v0.24.0 wrote, kept byte-precise
because the migrator's frozen decoder reads it (and because v7 carries the
four index sections' byte layouts over from it unchanged). The original v0.3
design proposal is retained at the bottom as history. **The proposal does not
describe the format that shipped** — it predates the implementation and
differs in the layout, the section list, and the compatibility story. Read
the first section for the current format, the second when working on the
migrator, and the third only for the reasoning.

# Format v7

**Status: the shipped format.** This section describes what is on disk and
is the contract the implementation is held to — the format acceptance tests
(`tests/segment_format_acceptance.rs`) are written against these tables, and
a divergence between spec and code is resolved by deliberately amending one
of them, never by letting them drift. The amendments made while implementing
are written into this section where they apply, each labeled as such; the
two load-bearing ones are that the free-space precheck was removed from the
migration contract, and that candidate confirmation at the segment layer
moved from a payload parse to a digest-pair compare (with the store layer
holding the parse authority).

v7 is the first bump made under [the policy below](#the-policy-from-here),
and it pays point 3 in full: it ships with an automatic, resumable migrator,
and the v6 decoder lives inside that migrator rather than surviving in the
query path. The one readable version is 7; a v6 file is something the store
converts at open, not something a query ever sees.

Three things changed from v6 — the records region, the payload store, and
the header that describes them. **The four index sections and the record
encoding's framing keep their v6 byte layout**: the byte tables for the
attribute and content indexes live in the
[historical v6 section](#format-v6-historical--the-migration-source) and
remain normative for v7, carried over unchanged. The WAL, the
annotation/eval/tombstone logs, and the rollup sidecar formats are
untouched.

The motivation is measured, at `65652a2` on the `storage-bench` corpora —
the run recorded in [benchmarks/storage.md](benchmarks/storage.md), whose
region-measurement footnote carries the figures cited here:

- **The records region is where the bytes are**: 89.6% of segment bytes on
  `generic`, 95.3% on `llm`, 91.9% on `pinned-context`, corpus-wide, and no
  individual segment file above 95.3%. Measured from the kept corpora of
  that run (`TRAZA_STORAGE_BENCH_KEEP=1` leaves the data directories on
  disk; each file's header gives its records region at bytes 24 and 32).
- **A third of the LLM corpus's record bytes are a structural
  double-store**: every indexed attribute value is held once inside the
  span's JSON payload and again, verbatim, in the record's own key/value
  list. Counting the value text alone — key names and length prefixes
  excluded — the duplicated text is 36.1% of `llm`'s records region, 25.6%
  of `pinned-context`'s, and 7.4% of `generic`'s. The double-store grows
  with exactly the workload this store exists for.
- **Compressing the records region is worth a multiple, not a percentage.**
  The [projection in storage-comparison](storage-comparison.md#what-compression-would-buy)
  — labeled a projection there and here — puts block-wise `zstd -3` at a
  6.30x/11.59x/9.27x segment shrink across the three corpora. v7 uses LZ4,
  which trades ratio for speed and safety of implementation, and cuts
  blocks at 128 KiB where the projection was computed at 64 KiB — so those
  numbers are an optimistic reference measured under different knobs, not a
  forecast of v7's. The [acceptance gates](#acceptance-gates) below, not
  the projection, are what the implementation must meet.

## v7 records: digests instead of value text

A v7 record is:

| Field | Size |
|---|---|
| timestamp | u64 |
| trace-id length | u32 |
| attribute count | u32 |
| payload length | u32 |
| reserved, zero | u32 |
| trace id | trace-id length bytes |
| attribute pairs | attribute count × (key id `u32` + value digest 16 bytes) |
| payload | payload length bytes |

The framing is v6's; what changes is the attribute list. v6 stored each pair
as two length-prefixed strings — `4 + key + 4 + value` bytes, with the value
in its canonical JSON form. v7 stores a fixed 20 bytes per pair: the key id
indexes the attribute-index section's key dictionary, and the digest is the
same 128-bit `(key, value)` digest the attribute index posts under
(`src/hash.rs`). Pairs are written in ascending key-id order, which is the
order v6's `BTreeMap` iteration already produced, so encoding stays
deterministic.

**The value text is not lost; it was never only here.** The payload is the
store's own serialization of the span, and the v6 key/value list was derived
from that same span at seal time (`span_to_record` in `src/lib.rs`): user
attributes minus NUL-prefixed keys, their values canonicalized to JSON text,
plus the reserved `service`/`name`/tenant entries carrying their raw text —
only user attributes are JSON-canonicalized, and the derivation, that
asymmetry included, is the definition. That derivation becomes a
**format invariant**: for every v7 record, applying it to the parsed payload
must reproduce the stored `(key id, digest)` list exactly. An acceptance test
pins it by re-deriving the pairs from every payload in a randomized corpus —
it is what makes the digest list droppable text rather than lost data.

**Candidate verification is two layers, and the parse authority lives in the
store.** This paragraph is deliberate amendment #2 to the original
specification, made under this document's own drift rule: the spec as first
written said segment-layer confirmation "parses the payload, applies the
same derivation to the queried key, and compares the result." The
implementation deliberately does not do that at the segment layer, and the
contract now says what it does do:

- **The segment layer confirms by digest pair.** `Segment::query_attribute`
  and the `record_carries_attribute` prefilter compare a candidate against
  the `(key id, digest)` pairs the record ITSELF carries — a 20-byte
  compare, no parse. This discards every forged or corrupt index posting
  (an entry pointing at a record that never held the pair), and it
  preserves the economics the original text wanted: the payload parse is
  paid only on digest matches. What it cannot see through, by construction,
  is a true 128-bit collision — a colliding pair sits in the record bytes
  exactly as an honest one does. `Segment::query_attribute` on its own is
  therefore digest-confirmed, not collision-safe, and its doc comment says
  so; nothing in the serving path uses it bare.
- **The store layer holds the authority.** Every result the STORE returns
  is verified against the value derived from the record's parsed payload —
  `span_matches` and its relatives in `src/lib.rs` — under the same
  derivation `span_to_record` defines: canonical JSON for a user attribute,
  raw text for the reserved `service`/`name`/tenant keys, which is already
  the form the probe side builds its needles in. No digest forgery can
  satisfy that comparison, so a collision costs a wasted decode and can
  never produce a wrong row.

The semantics do not move: an index still only narrows a filter. The
forged-collision acceptance test proves each layer at its own job — the
posting-only forgery is discarded by the segment layer's digest compare,
and a full forgery (the colliding pair planted in both the record bytes and
the index, the shape a real collision has) is admitted by the segment layer
and rejected by the store's payload-derived verification. The parse cost
was already paid by every true match (results are returned as spans), so
the parse authority costs extra only on false candidates, which digest
probing makes rare.

The pairs stay in the record rather than vanishing entirely for three
reasons. A full scan can reject a record against an attribute predicate on a
20-byte compare before parsing its payload — false positives possible,
false negatives not, so the payload parse remains the authority. A segment
rewrite can rebuild the attribute index from records without parsing every
payload. And the record remains self-describing against its index entries,
which is what erasure's segment verification walks.

## The records region: record-aligned compressed blocks

The records region is carved into **blocks**. Not the content index's blocks
— those are 128 *records*; to keep the two apart this section always says
**compression block**. A compression block:

- targets **128 KiB of uncompressed record bytes**: the writer appends whole
  records until adding the next record would cross 128 KiB, then cuts the
  block. A block always holds at least one record, so a single record larger
  than 128 KiB becomes a block by itself. **No record ever spans two
  blocks**; a reader may treat a record that would as corrupt.
- is **independently compressed** with the codec the header names — LZ4
  block format (not the LZ4 frame format), as `lz4_flex`'s block API
  produces it, with no length prefix of its own: the directory carries the
  lengths.
- carries a **raw-passthrough flag** for incompressible blocks: if the
  compressed output is not strictly smaller than the input, the block is
  stored as its raw bytes and flagged. This bounds the worst case at
  raw size plus the directory, and it is why already-compressed payload text
  cannot make v7 larger than v6's records were.

The stored region is the concatenation of the blocks' stored bytes —
compressed or raw — with no per-block framing inside the region; the block
directory is the only framing. A segment whose header declares codec 0 (raw)
is carved and directed identically, with every block stored raw; logical and
physical offsets then coincide, and what the segment keeps is the per-block
CRC and the directory's timestamp fence. That uniformity is deliberate:
there is one reader shape, and writing uncompressed segments is a codec
choice, not a format variant. **The raw codec is format-supported but not
operator-exposed in v0.24.0** — a deliberate ruling, not an oversight: every
segment the store writes is encoded under LZ4 (`encode_with` hard-codes it),
`segment::encode_with_codec` is the library surface that writes codec 0, the
acceptance tests exercise it, and the reader accepts both. An operator flag
is deferred until someone asks for one; if it ships, it belongs to
[configuration](configuration.md), which names flags so this document does
not have to. An earlier draft of this sentence asserted the flag as if it
existed — amended here under the drift rule.

One derived constraint: the raw flag lives in the directory's stored-length
word (below), so **one record's encoding must be smaller than 2^31 bytes**.
That is a new bound, not a restatement of an old one: v6 caps individual
fields at `u32` — trace id, payload, attribute count (`encode_record` in
`src/segment.rs`) — and nothing in it bounds the record as a whole, so a v6
record assembled from several large fields can legally exceed 2^31. (The
server's 64 MiB request cap keeps ingest far below it, but that is a server
limit, not a store one.) The v7 encoder rejects an oversized record with the
`TooLarge` error class, naming the record by trace id and timestamp, and the
migrator inherits the rule: a v6 record whose v7 encoding reaches the bound
fails migration as a named `Store::open` error — the encoder's `TooLarge`
wrapped with the one thing the encoder cannot know, which segment file —
never a silent skip. No plausible span comes near 2 GiB in one record; the
point is that if one ever does, the failure is loud and actionable rather
than a record quietly missing from the migrated store.

## The block directory

A new section, **uncompressed**, holding one 32-byte entry per compression
block, in block order:

| Field | Size | Meaning |
|---|---|---|
| logical start | u64 | offset of the block's first byte in the *uncompressed* records region |
| stored offset | u64 | offset of the block's first stored byte, relative to the records section start |
| stored length | u32 | bytes as stored; **bit 31 set = raw passthrough**, remaining 31 bits are the length |
| crc32 | u32 | CRC-32 (the IEEE/gzip polynomial) over the stored bytes exactly as they appear in the file |
| min timestamp | u64 | timestamp of the block's first record |

Validation at open, all of it mandatory: logical starts strictly increasing
from 0; stored offsets strictly increasing from 0; the masked stored lengths
sum to the header's records length; the last block's logical extent ends at
the header's records *logical* length; entry count × 32 equals the section
length. **Every block's logical extent is bounded before it can size an
allocation**: a block holding more than one record spans at most the 128 KiB
carving target, a single-record block stays below 2^31 (the record bound),
and a compressed block's extent cannot exceed LZ4's ~255x expansion ceiling
of its stored bytes. The bounds exist because the extents derive from header
and directory words no checksum covers, and the reader allocates a block's
extent before decompressing into it — a forged length must be `Corrupt`,
never a gigantic allocation that aborts the process. The CRC is checked on
every block read, before decode — it is why the field exists, and a mismatch
is `Corrupt` naming the block. A block's decoded size must equal its logical
extent; anything else is `Corrupt` too.

**The min-timestamp column is verified against the records, not trusted.**
It is derived metadata the CRCs do not cover, and it steers the window
search without a decode, so a corrupt-but-still-sorted fence could otherwise
shift a window bound silently — returning out-of-window rows or dropping
in-window ones. Two checks close it, both mandatory: every decoded block's
first record must carry exactly its directory entry's min timestamp (the
byte-resident open decodes every block eagerly, making this an open-time
check on that path), and the window search verifies each landed bound
against the records on both sides of it — at most two timestamp reads,
usually inside the block the search just decoded. Either disagreement is
`Corrupt`, never a shifted answer. The spec's first draft omitted this
column from the mandatory list; amended here under the drift rule after
review demonstrated the silent-wrong-window consequence.

**Posting lists keep their u64 currency as LOGICAL offsets** into the
uncompressed records region — the record-offset index, the trace index, and
the attribute index are byte-for-byte v6 encodings whose values simply mean
"offset before compression". The translation lives entirely in the reader:
binary-search the directory's logical starts for the containing block, read
its stored bytes, verify, decode, and index into the decoded buffer at
`logical − logical start`. A record's byte length is still the gap to the
next entry in the record-offset index — the last record ending at the
records logical length — which is v6's consecutive-offsets rule restated in
logical bytes. Whether a reader caches decoded blocks is an implementation
choice the format does not constrain; the format guarantees only that one
block decode suffices for any record.

CRC-32 over the gzip polynomial is already in the crate: the WAL frames
every record under exactly this polynomial. The directory shares that
implementation — hoisted into `src/crc.rs`, where the WAL, the directory,
and the blob header all reach it, not written a second time — and it costs
no dependency.

Because records are timestamp-sorted (the v6 invariant, unchanged), a
block's first record carries its minimum timestamp, and the directory's
min-timestamp column is a sorted array. A window disjoint from the segment
never gets this far in either version: the header's timestamp range answers
it (`may_contain_timestamps` in `src/segment.rs`), exactly as in v6. For a
window that does overlap, `ordinal_range_for_window`'s binary search paid
log2(n) 8-byte reads into the v6 record region; the v7 probe narrows to a
block through the resident directory — no read, no decode — and pays at most
a boundary-block decode to place the exact bound.

The directory is read at open and held resident: 32 bytes per 128 KiB of
uncompressed records, so a segment with 256 MiB of uncompressed record bytes
carries a 64 KiB directory — arithmetic from the constants, not a
measurement. That is the resident price the FORMAT imposes. The reader adds
one implementation cost on top: a small decoded-block cache per open segment
(`BLOCK_CACHE_SLOTS` in `src/segment.rs`, four blocks — nominally 512 KiB
once queries have touched the segment, bounded by the largest record rather
than by 128 KiB when single records exceed the carving target, and retained
for the segment's life). That residency is counted by
`Store::resident_payload_bytes` rather than hidden; an earlier draft of this
paragraph said "nothing else new is held in memory", which was true of the
format and false of the reader, and is amended here.

## The v7 header

Little-endian, fixed offsets, `header length = 128`:

| Offset | Size | Field |
|---:|---:|---|
| 0 | 8 | magic, `TRAZASEG` |
| 8 | 2 | format version (`7`) |
| 10 | 2 | header length (`128`) |
| 12 | 4 | **codec id: 0 = raw, 1 = LZ4 block format** |
| 16 | 8 | record count |
| 24 | 8 | records offset |
| 32 | 8 | records length, **as stored** (compressed) |
| 40 | 8 | record-offset index offset |
| 48 | 8 | record-offset index length |
| 56 | 8 | trace index offset |
| 64 | 8 | trace index length |
| 72 | 8 | attribute index offset |
| 80 | 8 | minimum record timestamp |
| 88 | 8 | maximum record timestamp |
| 96 | 8 | content index offset |
| 104 | 8 | block directory offset |
| 112 | 8 | block directory length |
| 120 | 8 | records **logical** length (uncompressed) |

Offsets 16 through 103 are v6's fields at v6's positions; the codec id takes
the u32 that v6 reserved as zero, and three u64s are appended. The records
*logical* length exists because the directory entry has no room for the last
block's uncompressed size and because record-offset validation needs the
logical bound (`offset < logical length`, exactly as v6 checked against
`records_len`).

Section order in the file: header, records (stored), **block directory**,
record-offset index, trace index, attribute index, content index to EOF. The
v6 contiguity rule extends to the new section: every section starts where
the previous ends, the attribute index is still bounded by the content
offset, the content index still runs to EOF, and trailing bytes are still
refused. An empty segment has empty records, an empty directory, logical
length zero, and otherwise encodes as v6-empty did.

**The codec id is parameterization, not a version.** A reader refuses an
unknown codec id with an error that names it — the same shape as the version
refusal, and for the same reason: decoding bytes under the wrong codec
produces garbage, not errors. Adding a codec (the evidence-gated zstd cold
tier, if it ever clears its gate — see
[dependencies](internals/dependencies.md)) is configuration plus a new id,
not a format bump: v7 readers built after it read both, and the cost is
confined to stores that opt in.

## What stays raw, and why

The header and all four index sections — record offsets, trace, attribute,
content — are stored uncompressed. The reader parses them at open and probes
them per query: posting lists are probed by digest and intersected, content
rows are read by exact byte range, the offset table is the scan path.
Compressing them would put a decode in front of every probe, and it would
buy little: [storage-comparison](storage-comparison.md#what-compression-would-buy)
already makes the argument — compressing the index sections "would flatter
the number and break the design". After v7 the indexes become the majority
of segment bytes (the projection's tables show 55–75% post-compression, by
corpus); shrinking *them* is future index-format work, not a codec knob.

The WAL and the rollup sidecars also stay raw, deliberately: erasure
verification runs byte-level occurrence scans over both (see
[administration](operations/administration.md#erasure-deletion-with-a-receipt)),
and those scans work precisely because the bytes on disk are the literal
bytes. The same goes for `annotations.jsonl`, `evals.jsonl`, and
`tombstones.jsonl`. Compression is confined to the two places the
measurement says the bytes are: segment records and payload blobs.

## Payload blobs

A v7 blob in `payloads/<aa>/<sha256>.bin` is a 24-byte header followed by
the stored body:

| Offset | Size | Field |
|---:|---:|---|
| 0 | 8 | magic, `TRZBLOB1` |
| 8 | 4 | codec id: 0 = raw, 1 = LZ4 block format |
| 12 | 8 | uncompressed length |
| 20 | 4 | crc32 over the stored body bytes |
| 24 | — | body |

The writer compresses each blob independently and falls back to codec 0 when
compression does not strictly shrink it. There is **no random access inside
a blob**: blobs are consumed whole — `GET /v1/payloads/…` streams the
decoded bytes, dedup compares nothing but names — so one codec unit per file
is the right shape and block carving would be waste.

**The uncompressed-length word is bounded before it is believed.** The
crc32 covers the stored body only, so the header's length field is protected
by nothing — and the decoder allocates the declared length before inflating
into it, which would turn one flipped header byte into an
allocation-failure abort on the read path. A declared length beyond LZ4's
~255x expansion ceiling of the body is `Corrupt`, naming the file, checked
before any allocation; the decoded length must then still equal the
declared one exactly.

**The content address does not move: it is the SHA-256 of the UNCOMPRESSED
bytes.** Dedup therefore behaves exactly as v6's — an identical pinned
context still collapses to one file under the same name — and erasure's
literal reference needles (`sha256/<hex>` strings in spans, logs, and eval
records) are unchanged bytes matching unchanged names. What does change:
the file's bytes no longer hash to its name, so any check that verified a
blob by hashing the file (`sha256_file`) must decode first; blob
verification in v7 is header parse, CRC check, decode, then SHA-256 of the
decoded bytes against the name.

## Migration: v6 → v7

The contract, per [the policy](#the-policy-from-here): **automatic at first
open, resumable, and never woven into the read path.** `Store::open` on a
v0.24.0 build converts a v6 store before serving anything; the v6 decoder
exists only inside the migrator module.

- **Same path, temp + fsync + rename.** Each v6 segment is decoded, its
  records re-derived through the same span → record derivation ingest uses
  (parse payload, derive attributes and content text — both are recoverable
  from the payload, which is what makes this a re-encode and not a lossy
  copy), and re-encoded as v7 **onto the same file name**. Name preservation
  is not hygiene; [segment path order IS recency
  order](internals/invariants.md), and a migrator that assigned fresh ids
  would reorder last-write-wins. The content index is rebuilt under the
  store's current content-index configuration, exactly as compaction already
  behaves.
- **Rollup sidecars are rebound in the same step.** The sidecar's binding
  includes the segment's byte length, which the rewrite changes, so every
  existing sidecar becomes self-invalidating — correct, but a rebuild storm
  at first query. The migrator instead computes the rollup from the records
  it already has decoded and writes a freshly bound sidecar beside each
  migrated segment. The write order of sidecar against segment rename is
  deliberately unspecified, so a crash in that window can leave a sidecar
  bound to a segment that no longer matches it; resume does not repair this,
  because it does not need to — a stale-bound sidecar self-invalidates and
  rebuilds on first use, a performance cost, never a wrong answer.
- **Blobs are rewritten in the same pass**, onto the same names, by the same
  temp + fsync + rename discipline, so that the reader accepts exactly one
  format of each — no header-sniffing in the serving path, ever. The blob
  pass classifies each file by a three-way rule, never by its magic alone.
  A file that passes full v7 validation — magic, CRC, decode, SHA-256 of the
  decoded bytes against the name — is already migrated and is left alone. A
  file that fails that, but whose **raw** bytes SHA-256 to the file name, is
  a v6 blob and is migrated; a v6 blob whose content merely happens to begin
  with the magic bytes lands here, which is why magic alone was never the
  test. A file that fails both is `Corrupt`, named per file, and the
  migrator **refuses to rewrite it** — rewriting would launder bytes that
  match neither format into a validly framed v7 blob. That refusal fails
  the whole open, deliberately: the store serves nothing until it holds
  one format, so there is no "serve around the bad blob" state to fall
  back to. The pass collects every unrecognized file before refusing, so
  one report covers them all, and the next open after the operator acts
  resumes the migration from where it stopped — a pending erasure settles
  after that open, not before.
- **Pinned backups are migrated the same way.** A pin is a hard-link farm
  that keeps pre-rewrite inodes alive, so after the live pass every pin
  still holds v6 bytes — bytes a v0.24.0 restore could not read. The
  migrator runs the same two passes inside each `pins/<label>` and then
  rewrites the pin's `state-manifest.json` digests to match, because restore
  verifies a pin against those digests before installing it. One crash
  window needs its own rule: between a pin's file pass and its manifest
  rewrite, every file in the pin already reads as v7, so the version-word
  trigger below sees nothing left to do — and a pin whose manifest still
  carries v6 digests fails every restore. Resume therefore does not trust
  the trigger for pins: every migration resume re-validates each pin's
  files against that pin's manifest digests, and where the files validate
  as v7 but the digests disagree, it redoes the manifest rewrite.
  "Validate" is load-bearing and means the WHOLE file: a digest-moved
  pinned segment is accepted only after an eager open that checks every
  block CRC and decodes every record — the lazy open reads only the header
  and index sections, and trusting it would launder a bit-flipped pinned
  block into a manifest that then verifies clean over garbage. A
  digest-moved manifested file the migrator cannot validate in any format
  — neither a segment nor a content-addressed blob — refuses the resume by
  name, for the same reason. The
  rewrite is idempotent, so re-validating a finished pin costs a hash pass
  and changes nothing. A pin already copied off-host is outside the data
  directory and outside this contract: it restores only under a build that
  reads its format, which the [backup guide](operations/backup.md) already
  says of every backup.
- **Completion is recorded in the manifest, by a checkpoint that re-hashes
  everything.** The migration's final act is a checkpoint whose manifest
  declares the store format; every later checkpoint carries the declaration
  forward. This checkpoint is exempt from the incremental rule ordinary
  checkpoints live by — the [backup guide](operations/backup.md) lets a
  checkpoint carry segment digests over from the previous manifest *because
  segments are immutable*, and migration has just violated that premise
  wholesale: every segment and blob was rewritten onto its same name. A
  carried-over digest is therefore a digest of bytes that no longer exist,
  and a completion checkpoint that carried one forward would publish a
  generation that fails verification everywhere — verify-at-pin fails, and
  restore is impossible. **The completion checkpoint is a full re-hash and
  is forbidden from carrying any digest forward.** For the same reason, the
  live generation's manifest is expectedly stale from migration start until
  that checkpoint: it describes the pre-migration bytes, and nothing during
  migration consults it. The checkpoint publishes `folded_through`
  unchanged from the prior generation — it folds nothing, because the
  migrator does not replay the WAL — and frames after `folded_through`
  replay normally against the migrated store afterward. The trigger rule at
  open: any segment (live or pinned) declaring v6 starts a full migration;
  all segments v7 but no manifest declaration re-runs the idempotent blob
  pass, re-validates every pin as above, and then checkpoints. Both passes
  are resumable by construction — every file conversion is atomic, a v7
  segment is recognized by its version word, a v7 blob by validation — so a
  crash at any point re-runs from where it stopped, converting only what
  remains.
- Migration runs before WAL replay and before any maintenance; the WAL is
  format-independent of the segments — binary-framed JSON batches under its
  own `TRZWAL02` magic (`src/wal.rs`) — and is neither read nor rewritten
  by the migrator. A pending erasure resumes after migration, against the
  migrated store.

**Costs, stated:** migration duration is proportional to store size — every
record and every blob is decoded and re-encoded once, plus once more per
pin. There is a single writer and there are **no reads during migration**;
the store serves nothing until it holds one format. Pins also cost disk,
permanently: rewriting a pinned file allocates a new inode, and the migrator
**does not re-link identical outputs** across pins or against the live store
— detecting that two independently produced rewrites are byte-identical and
re-linking them is machinery this design declines to build, so after
migration every pre-existing pin is a full independent copy of its
generation. Two consequences an operator plans around: migration requires
free space for the live store's rewrite plus one full copy per pre-existing
pin, and the cheap-pin promise — the backup guide's "a pin costs almost no
disk" — stops holding for pins that pre-date the migration. The free space
is the operator's to check, not the store's. This section originally
demanded an up-front probe ("refused with a named error if absent, never
discovered at 90%"); the implementation deliberately amends that sentence —
per this file's own drift rule — because the standard library has no
portable free-space call, the `libc` it would take is a dependency the
[ledger](internals/dependencies.md) has not admitted, and running dry
mid-migration is not the hazard the sentence assumed: every conversion is
atomic onto its own name, so `ENOSPC` fails the open loudly at the write it
starved and the next open after space is freed resumes from exactly that
file. The probe would buy earlier notice of an already-safe condition at the
price of a dependency; if the ledger ever admits `libc` for other reasons,
it is worth adding then. One consequence the removed probe DID buy is owed
an honest sentence: refusal before the first rewrite left a store the
previous build could still serve, whereas migration is one-way from the
first converted file — after a mid-migration `ENOSPC` the store holds mixed
v6/v7 files that only a v0.24.0+ build can finish, and since nothing serves
during migration, the store is down until disk is freed. Data-safe, not
availability-safe: an operator short on disk checks free space, or releases
pins, BEFORE the first v0.24.0 open.
The backup guide's cheap-pin sentence was v6-era text this spec did not
edit; the implementing PR amends [backup.md](operations/backup.md) alongside
this section. The cheap way out is
procedural: release pins before migrating and re-take them after, when the
disk cannot afford the copies — a fresh pin of the migrated store hard-links
v7 files and costs almost nothing again. Un-pinned historical generations
stop being verifiable once their files are rewritten, exactly as they do
after any compaction; the post-migration checkpoint is the first generation
that describes the v7 store. An operator who wants a fallback takes a backup
first, by one of the two procedures in the
[durability guide](operations/durability.md#backups) — the same sentence
every migration section in this file has ever said.

## Erasure and verification under v7

Erasure's segment domain was never a byte scan, and v7 does not make it one.
v6 verifies segments **through their indexes**: an index-driven walk selects
candidate records and decodes exactly those (`subject_keys_in_segment` in
`src/lib.rs`); the byte-level occurrence scan (`count_occurrences`) runs
over the WAL and the rollup sidecars only. v7 keeps that shape — the walk
now reads each selected record through its block's decode, and the
semantics do not move. What compression changes is only how emphatically
the rationale holds: a raw byte scan of a segment file proved nothing
against v6's binary encoding, and against LZ4 output "I hex-grepped the
file and found nothing" is not even a scan of the record bytes — which is
why the receipt names the domain as index-verified rather than scanned.
The WAL and rollup-sidecar byte scans are unchanged because those file
*formats* are unchanged: the migrator rewrites every sidecar's contents,
but the rewritten bytes are the same raw format the scan reads; only the
WAL file itself is untouched. Blob checks decode before hashing, as above.
A settle receipt taken before migration cites generations whose digests
stop verifying when migration rewrites their files — the loss every
un-pinned historical generation takes, and no worse: the finding is
re-derivable by re-running verification against the migrated store, and a
pin taken at settle time preserves the cited generation if an audit needs
the original bytes. The in-place segment rewrite that a purge performs
writes v7 through the ordinary encoder; nothing about the erasure
contract's ordering, its receipt semantics, or the tombstone log moves.

## Determinism: compressor output is format bytes

The v6 rule that encoding the same records twice yields identical bytes is
kept, and it now covers the compressor: **the codec's output bytes are
format bytes under the acceptance tests.** Reproducible merges and the
byte-comparing format tests depend on it. Therefore the codec crate is
**pinned to an exact version** (`=` in `Cargo.toml`), and upgrading it is a
deliberate re-baseline: a commit that bumps the pin, regenerates the golden
bytes, and says so — never a routine dependency bump. The pin is about
encoder stability only; any correct LZ4 decoder reads any valid stream, so
readability never depends on the pinned version. The dependency itself is
argued in [internals/dependencies.md](internals/dependencies.md).

## Acceptance gates

The implementation is held to these, each with an executable oracle:

1. **Round trip**: encode/decode equality on randomized corpora, including
   blocks that trip the raw-passthrough flag and records larger than one
   block.
2. **Derivation invariant**: for every record in a randomized corpus, the
   `(key id, digest)` list re-derived from the parsed payload equals the
   stored list byte for byte.
3. **Collision safety**: the forged-collision tests carry over and cover
   both layers of amendment #2 — a forged index posting is discarded by the
   segment layer's digest compare, and a collision planted in the record
   bytes AND the index (the shape a real collision has) is rejected by the
   store's payload-derived verification. A fabricated collision must cost a
   wasted decode, never a wrong row.
4. **Migration**: a v6 store (segments, blobs, sidecars, pins, non-empty WAL)
   opened by v0.24.0 serves query-identical results, preserves segment
   names, and survives `kill -9` at arbitrary points mid-migration with a
   clean resume — the crash-point harness the durability tests already use.
   The migrated store must also prove its manifest: the post-migration
   generation verifies clean (`/v1/verify` reports `intact: true`) and a
   pin taken immediately after migration passes its verify-at-pin. That
   half of the gate is what catches a completion checkpoint publishing
   digests of bytes the migration replaced.
5. **Storage**: settled amplification **at or below 1.0x** on the `generic`
   and `llm` corpora, measured by `storage-bench` and recorded in
   [benchmarks/storage.md](benchmarks/storage.md).
6. **Latency**: measured, stated, and bounded — not hidden inside a relative
   percentage. *(Deliberate amendment #3: this gate originally demanded p95
   within 10% of v6. The profiled floor is structural and the amendment
   records it rather than shaving the benchmark to pass: a v7 point read
   pays one 128 KiB block read + CRC + inflate — measured ~+11 µs on a
   trace lookup (49 µs vs 38 µs, in-process A/B over a settled
   million-span store) — where v6 paid a handful of ~500-byte reads, and a
   limit-100 attribute filter at sparse candidate spacing inflates ~38
   blocks (~1.23 ms vs 0.64 ms on the same A/B). A relative gate on a
   double-digit-microsecond operation measures machine noise, not the
   format.)* The gate as amended: on the canonical corpus, interleaved
   against the v6 baseline on the same machine in the same session,
   **trace-lookup p50 ≤ 0.75 ms and attribute-filter p50 ≤ 6 ms absolute**
   (generous tripwires over the measured medians, in the house style —
   tripwires, not oracles), **ingest throughput not below the v6
   baseline** (measured +22%: compression writes fewer bytes), and the
   measured medians of both sides published beside each other in
   [benchmarks](benchmarks/) with the block-decode cost stated plainly.

Gates 1 through 4 are enforced by the test suite on this branch. Gates 5
and 6 are measurement gates: their runs, and the rewrite of
[benchmarks/storage.md](benchmarks/storage.md) that records them (that
file's tables and region-measurement recipe still describe the v6-era run
this section cites as motivation), land with the gates PR. Until those
recorded numbers land, every ratio in this section remains a projection —
including the development-run figures the changelog cites as unpublished
measurements.

---

# Format v6 (historical — the migration source)

**Status: historical.** This is the layout every release before v0.24.0
wrote. No shipped build writes it any more, and the only code that reads it
is the migrator's FROZEN v6 decoder (`src/migration.rs`, copied from
`src/segment.rs` at `5f23172`) — this section is that decoder's reference,
kept byte-precise for exactly that reason. Two parts of it remain live
beyond the migrator: the **attribute index** and **content index** tables
below are carried into v7 unchanged and stay normative there, and the
versioning/policy subsections at the end apply to the format as a whole.

A v6 segment is one file named `segment-<20-digit id>.seg`, written temp +
fsync + atomic rename. It is a fixed 104-byte header followed by five
contiguous sections, in this order, with no gaps and nothing trailing:

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

## Record order is an invariant, not a convention

Records are stored in ascending timestamp order, and `encode_with` establishes
that itself — it stable-sorts by timestamp rather than trusting the caller.
The sort is by timestamp alone, so a caller's finer tie-break survives it and
already-ordered input encodes to byte-identical output.

The enforcement is load-bearing because `Segment::ordinal_range_for_window`
**binary-searches** the record region to turn a time window into a contiguous
ordinal range, which is what lets a windowed query decode a slice instead of
the whole segment. A binary search over unordered records does not fail
loudly; it returns the wrong records. Every writer in the store already sorted
before encoding, so nothing changed for them — what changed is that a future
one cannot silently break the search. Pinned by
`records_are_stored_in_ascending_timestamp_order_whatever_order_they_arrive_in`
in `tests/segment_format_acceptance.rs`, which encodes a shuffled corpus and
cross-checks the range search against a brute-force filter at every bound.

## The attribute index

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

## The content index

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

## Versioning

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

**Versions 1 through 6 are spent.** 1 was JSONL. 2 was written by v0.16 and
v0.17, 3 by v0.18 and v0.19. **4 and 5 were never released** — they existed only
on unreleased `main`, so no tag writes them and no tag reads them. 6 was
written by every release after v0.19 and before v0.24.0. None of the six is
written now, and none opens for serving: 2 through 5 are refused outright,
and a 6 is read exactly once — by the migrator, which converts it before
anything is served. The README's pre-1.0 terms permit an on-disk break
between 0.x versions, and the 2-through-5 cut was one: `Store::open` refuses
such a segment and names it, never advising deletion.

## Migrating between formats

**Take a backup by one of the two procedures in the
[durability guide](operations/durability.md#backups):** stop the server and copy
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
the store's independent recovery domains for it to pin. Closing that is what
the generation/checkpoint boundary is designed for.

So export-and-reingest is a complete migration only for a store with no
offloaded payloads and no annotations. Otherwise: back up, and read the backup
with a build that can read it.

**Which build?** For a v6 store: none — no manual step exists. Any v0.24.0+
build migrates v6 automatically at first open, per the
[Migration section](#migration-v6--v7) of the v7 spec above; back up first,
open, done.

For anything older, not "the release that wrote this segment". A store
accumulates segments in whichever format was current when each was sealed,
so one directory can hold several formats at once, and a release that reads
the oldest of them cannot read the newest. Formats 4 and 5 were never tagged
at all.

One commit reads every indexed format this project retired WITHOUT a
migrator — 2 through 5 —
and that is what the error names: **`cf40bea`** (`MIN_READABLE_VERSION` 2,
`VERSION` 5), exposed as `traza::LEGACY_SEGMENT_READER`. Build it, point it at
the backup, and export from there. Format 1 was JSONL; it is refused separately
and needs 0.3.x.

## The policy from here

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

Point 3 is the part the v6 cut did not pay for, and that was worth stating
rather than hiding: a migrator from v2/v3/v5 would have meant resurrecting
precisely the decoders just deleted, to serve stores that did not exist. The
debt was declined once, on the last occasion it could be declined cheaply —
and v6 is where it started being paid: the v6 → v7 bump ships with exactly
the migrator point 3 demands, automatic at first open and resumable, with
the v6 decoder frozen inside it (the [Migration section](#migration-v6--v7)
above is its contract).

---

# Original design proposal (v0.3, historical)

Everything below is the proposal as written before implementation. It is kept
for the reasoning behind the design, not as a description of the format. Where
it disagrees with the sections above — notably the trailing-footer arrangement,
the JSONL payload, and readable v1 segments — the sections above are correct.

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
   BENCHMARKS-10M.md style notes (do not overwrite canonical-corpus.md).

## Non-goals

Columnar payload encoding, compression, OTLP, auth, replication — all stay
on the roadmap. The JSONL payload stays byte-identical to v1 precisely so
this change is index + residency only.
