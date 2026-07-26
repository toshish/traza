# Invariants

These are load-bearing and easy to break without noticing. Every one of them is
already the reason some piece of code looks the way it does. Read this before
changing the engine.

Each entry says what the rule is, why it exists, what breaking it looks like,
and where it lives.

---

## 1. Segment path order IS recency order

**The rule.** Segments are sorted by path, and a later path means a later
flush. Reads resolve the `(trace_id, span_id)` primary key by treating a later
segment as newer, so the ordering of the segment list *is* last-write-wins.

**Where.** `Store::open` and `flush_locked` both `segments.sort_by(|l, r|
l.path.cmp(&r.path))`. Segment files are named `segment-<20-digit id>.seg` with
a zero-padded id precisely so lexical path order matches numeric id order.

**What breaks it.**

- Assigning a segment id when a write *finishes* rather than when the buffer is
  *drained*. Two overlapping seals completing out of order would silently
  invert last-write-wins.
- Merging a run of segments from the **middle** of the list. A merged segment
  takes a fresh (highest) id and therefore lands at the newest position; that
  is only sound if the run it replaces was already at the tail. Merging from
  the middle promotes those spans past segments that legitimately supersede
  them. `compact_segments` only ever compacts the tail for this reason.
- Any naming scheme whose lexical order diverges from its recency order —
  including dropping the zero-padding.
- **Rewriting a segment under a NEW id.** Expiry rewrites the segments it
  touches; giving the survivors a fresh (highest) id would move an old segment
  to the newest position and let its versions win over segments that
  legitimately supersede it. `expire_before` renames the replacement onto the
  same name for exactly this reason.
- **Claiming a merged segment's id after the merge instead of before.** The
  merge runs without the segment lock, so a flush can publish while it works.
  The ids are claimed under the lock at pin time, which is what guarantees
  every segment appearing afterwards sorts *after* the merged outputs — it was
  written later, and must win.
- **Handing a merge's outputs ids out of group order.** A run merges into a
  group of outputs, one per consecutive slice of the run, and dedup happens
  *within* a slice — so a key written in two slices lands in two outputs. The
  ids are claimed as one contiguous block and assigned in slice order, which
  is what makes the later output — holding the later version — sort after the
  earlier one, exactly as its source segments did.

**Symptom.** A re-ingested span reverts to an older version, at some point
after a compaction or a concurrent flush. Nothing errors.

---

## 2. Lock discipline: maintenance, then writer, then segments; the rollup cache is a leaf

**The rule.** Whenever more than one is needed, acquire `maintenance` first,
`writer` second and `segments` third, and hold that order until every guard
drops. The `rollups` cache is **leaf-level**: acquired briefly, released
immediately, and never held while taking another lock.

`maintenance` serializes the two operations that replace segment files —
compaction and expiry — against each other, and nothing else. It is not a read
lock and not an ingest lock: both run throughout. It exists so each rewriting
operation can pin its inputs, do its I/O with no engine lock held, and publish
under a short revalidated critical section without also having to reason about
the other one running concurrently.

**Where.** Documented on the `Store` fields in
[`src/lib.rs`](../../src/lib.rs); the helpers are `lock_maintenance`,
`lock_writer` and `lock_segments`. `expire_before` takes the maintenance lock
and delegates to `expire_before_locked`, so `compact_expired` can hold it once
across both halves without deadlocking on a re-entrant acquire.
`compact_segments` touches `rollups` only after its segment work, to drop
entries for merged-away inputs; `expire_before` drops the entry for each
segment it rewrites, because a rollup is keyed by path and that path now holds
different bytes.

**What breaks it.** Any new code path that reaches for `segments` and then
`writer`, or that calls into analytics (which locks `rollups`) while holding a
guard. Both deadlock, and both deadlock *under concurrency only* — which means
a single-threaded test will not find it.

**Symptom.** The server stops answering. No error, no panic, no log line.

---

## 3. Payloads are never resident

**The rule.** Segments are file-backed. A loaded segment holds a file handle
plus its parsed indexes; record payloads are read from disk by exact byte range
on demand and are **not** retained.

**Where.** `Segment::open` reads only the header and the three index sections.
`write_segment` explicitly drops the encoded buffer and reopens the file
file-backed, so even the flush that just wrote a segment leaves no resident
copy.

Two API hooks exist purely to pin this and are asserted by the test suite:

- `Store::resident_persisted_span_structs()` — must be `0` after open and after
  flush.
- `Store::resident_payload_bytes()` — must be `0` after open and after flush.

**What breaks it.** Caching decoded records "for speed". Holding
`Vec<Span>` in `Segment`. Reading a whole segment to answer a query instead of
probing the index and reading the selected byte ranges. Any of these turns a
store that serves correctly at 100M spans into one that OOMs.

**Symptom.** Resident memory tracks corpus size instead of index size. Stores
larger than RAM stop working.

**What this invariant does NOT say.** It used to be titled "memory is
O(indexes), not O(data)", and that title claimed more than the invariant
proves. Indexes are resident, `attribute_index` is keyed on the **whole
attribute value**, and `Segment::open` decodes it eagerly — so for an indexed
prompt the index *is* the data, and resident memory is linear in it
(measured: RSS ≈ 1.44 × indexed text; see
[capacity](../operations/capacity.md#memory)). The two hooks above are
therefore necessary and not sufficient: both are zero no matter how large the
attribute index gets. `Store::resident_index_bytes()` reports the other half,
approximately. **Never cite `resident_payload_bytes() == 0` as evidence that a
store's memory is bounded.**

---

## 4. Write-temp → fsync → atomic rename

**The rule.** A segment is never written in place. Encode, write to a
`.tmp` file, `fsync` the file, `rename` it into its final path, then `fsync`
the directory.

**Where.** `Store::seal_segment`, reached through `write_segment` (a new
segment) and `rewrite_segment_in_place` (expiry replacing one). `write_segment`
checks the final path for existence first, so a segment-id collision is an
error rather than a silent overwrite; the in-place rewrite deliberately renames
*onto* an existing name, which is what keeps its position in recency order
(invariant 1) and what makes the replacement atomic without a journal. On any
failure the temp file is removed, and `Store::open` sweeps orphaned temps left
by an interrupted write.

**Why each step.** The rename is what makes the segment appear atomically — a
reader sees the complete file or no file, never a partial one. The file fsync
must precede the rename or the rename can be durable while its contents are
not. The directory fsync is what makes the rename itself durable.

**The same rule runs in reverse for deletion.** An unlink is visible
immediately and durable only once the directory entry it removed is synced, so
nothing may *act* on a file being gone — report the deletion, drop the segment
from the live list, reclaim the log that still holds its spans — before that
sync. `expire_before` and `merge_tail_run` both sync the directory after
unlinking and before anything downstream depends on it. Deletion is also
idempotent (`unlink_segment` treats `NotFound` as done), because a retry after
an unlink that landed without its sync must be able to finish rather than fail
forever on state that is already correct.

**What breaks it.** Writing directly to the final path. Skipping the directory
fsync. Reordering fsync after rename.

**Symptom.** A crash leaves a torn segment that either fails to parse (loud,
recoverable) or parses into wrong data (silent, not).

---

## 5. Compaction is crash-safe through the supersede journal

**The rule.** Before a rewrite replaces a segment, a marker file
`.supersede.<old>.journal` is written and fsynced, naming every output the
input is superseded by. It is deleted only after the original is removed.
Recovery at open follows **the journal, never the content.**

**Where.** `write_supersede_marker` / `recover_supersede_markers` in
[`src/lib.rs`](../../src/lib.rs), used by `compact_segments`. Expiry does not
need it: it renames the survivors onto the same name, so there is never a
window in which a replacement exists beside its original.

**The recovery rule.** If **every** replacement exists and parses, delete the
original — the crash landed between the last rename and the delete. Otherwise,
*if the original is still there*, roll the merge back: delete the replacements
that did land, and the original stays authoritative so the merge is simply
retried. The marker is removed either way. Outputs are written and renamed
into place **before** any input is deleted, so no window drops data.

**Why rollback, and why all-or-nothing.** A run merges into a *group* of
outputs (invariant 4), and each output holds only its own group's
last-write-wins view of a key while carrying a higher id than every input. One
left beside intact inputs would therefore shadow a newer version living in a
group whose output never landed. A partial group is not a smaller correct
merge; it is a wrong one.

**Why the original's presence is what licenses a rollback.** A merge deletes
its inputs only once every output is durable, so an input still on disk proves
the merge never committed. Gone proves it did — and then a missing output is
simply one a later merge has since consumed, with the rest of the group live.
Rolling those back because a marker outlived its merge is the one way this
journal could destroy data, and the check on the original is what rules it
out.

**Why not content-based healing.** An earlier version deduplicated segments by
inspecting their content, and that silently destroyed legitimately re-ingested
identical spans. Acknowledged duplicate cardinality must survive a restart.
Recovery must never guess from content.

**What breaks it.** Deleting an input before the replacement is renamed and
verified. Journaling after the fact. Reintroducing content inspection.

**Abandoning a merge is also journaled work.** The merge publishes under a
short lock and revalidates that its pinned inputs are still there; if they are
not, it deletes the replacement it wrote **before** deleting the markers.
Doing it in the other order leaves a window where recovery sees a complete
replacement and deletes inputs it does not actually supersede.

---

## 6. Reads take an atomic snapshot across the buffer and segments

**The rule.** A read acquires the writer lock and the segments lock (in that
order) and resolves its entire answer under that one snapshot. A concurrent
flush can therefore neither **hide** a committed span nor **duplicate** one.

**Where.** `get_trace`, `query`, `query_after`, `stats`, and the analytics
scan all take both guards up front and hold them across the whole resolution.
`query_view` and `attribute_union_view` are the resolutions themselves, written
against a buffer and a segment slice so the live store (holding both guards)
and a pinned `SnapshotView` (owning its copy) run the same code.

**Why it is subtle.** A seal puts spans into a new segment and later takes them
out of the write buffer. A reader that samples one side, releases it, and then
samples the other can observe the moment in between and see neither copy, or
see both. Only one snapshot spanning both is correct.

**And why the seal itself must cooperate.** Holding both locks for the read is
not sufficient on its own, because a seal now does its I/O with no lock held.
If it removed the spans from the buffer at the drain, they would be in neither
place for the whole of the write, and *every* read during that window would be
consistent, atomic, and missing acknowledged data. The seal therefore never
removes anything from visibility: the spans stay in the buffer until the
segment is published, and are evicted afterwards — by handle identity, so a
version re-ingested during the seal survives. See
`tests/seal_concurrency.rs`; the pre-existing suite passes with this broken.

The same trap applies to *multi-key* reads. Resolving each recognized session
key with its own `query` call let a span re-ingested between the calls be seen
first in its superseded version, which the per-key dedupe then locked in —
breaking last-write-wins during ordinary concurrent ingest. `spans_with_any` now
holds both locks across every key.

**And to multi-PAGE reads, where a lock cannot help.** A paginated read cannot
hold a lock between pages, so "one snapshot per call" is not enough: export
paged the live store, and a span re-ingested behind the cursor came back a
second time — output holding two versions of one primary key, under a trailer
saying `complete: true`. The answer is to stop re-reading the store.
`Store::snapshot` copies the write buffer and clones the segment `Arc`s, and
every page comes from that one pinned view. Segments are reference-counted and
each holds its own descriptor, so compaction and expiry may unlink the files
while a view is reading them; the bytes are reclaimed when the last reader
drops. **Any new operation that reads in more than one step must pin a view
rather than re-query.**

**Precedence, under that snapshot.** The write buffer holds the newest version
of anything it carries and wins outright; among segments, a later index means a
newer version. A candidate is emitted only if no higher-precedence source also
holds its key.

**Symptom.** Spans that briefly vanish or briefly double under concurrent
ingest. Single-threaded tests never see it — `flush()` is synchronous end to
end, so the window never opens. A test guarding this needs a concurrent writer.

---

## 7. An index accelerates a filter; it never changes it

**The rule.** After narrowing candidates through a segment's attribute or trace
index, **every predicate is re-verified against the parsed span.**

**Where.** `span_matches` is applied to decoded candidates on both the limited
and unlimited query paths.

**Why.** The index is a lossy accelerator by design. Attribute keys prefixed
with NUL are deliberately not indexed (the prefix is reserved for span fields),
index entries can over-select, and a filter with no usable index must still
return exactly the same rows. Trusting the index as the answer would make
correctness depend on index completeness.

**What breaks it.** Returning index hits directly. Skipping re-verification
"because the index already matched".

---

## 8. Both halves of the primary key must be non-empty

**The rule.** `trace_id` and `span_id` are both required to be non-empty, at
the HTTP surfaces *and* independently at the engine boundary.

**Where.** `validate_span` in [`src/lib.rs`](../../src/lib.rs); the per-span
checks in the `POST /v1/spans` handler.

**Why both places.** The engine check protects library consumers, who never
pass through HTTP. The HTTP check produces a better error (it names the batch
index) and rejects before any work is done.

**Symptom of breaking it.** Distinct spans sharing an empty id collapse into
one upserted key while the response counts them all as accepted — silent data
loss that looks like success.

---

## 9. The acknowledgement follows the fsync

**The rule.** In `wal` mode, a `200` is sent only after the batch's log
sequence number is known to be fsynced. In `flushed` mode, only after the
segment is sealed. In `buffered` mode, no durability is promised at all.

**Where.** `Store::admit` — the fsync is step 5, the response is after it. The
fsync is deliberately **outside** the writer lock so concurrent batches
coalesce into one sync; correctness does not depend on the lock, only on the
ordering of sync and response.

**What breaks it.** Acknowledging inside the lock before the commit. Making the
commit best-effort. Truncating the log at drain time instead of after the
segment lands.

**Symptom.** A mode that claims durability and loses acknowledged writes on
SIGKILL. `tests/durability.rs` is the oracle: it kills the process and checks.

---

## 10. The log is the recovery authority — deletions and bounds included

**The rule.** In `wal` mode the log, not the write buffer, decides what the
store contains after a restart. Three things follow, and each was a bug before
it was a rule.

**Deleting from memory is not deleting.** `expire_before` removes expired spans
from the buffer *and* rewrites the log to exactly what survived
(`Wal::rewrite`, staged and renamed so the survivors — still acknowledged —
cannot be lost to a crash mid-rewrite). Skip the rewrite and the next restart
replays the expired span and it is back. This is a retention bug and a deletion
bug at once: telemetry deleted on request must leave the log too.

**A quiet stop in replay is data loss.** A frame missing bytes it declared is
the interrupted final append and is dropped — and the file is truncated to the
last good frame, so those bytes cannot become interior bytes after the next
append. A frame that is complete but fails its checksum or its decode is
interior damage, and frames after it may be acknowledged batches: recovery
refuses to open rather than return the prefix as if it were the whole log.

**The log must be bounded by work, not by cardinality.** The flush policy
counts unique buffered records, upserts since the last seal, AND log bytes
(`should_flush`). Counting only records made the threshold unreachable for a
workload that keeps updating the same keys — the buffer stayed at one record
while the log grew without limit and restart replay grew with it.

**A failed deletion must stay retryable.** Durable state moves first, memory
second, and the step that moves memory cannot fail: the log rewrite before the
buffer `retain`, the unlink before the segment leaves the live list. The other
order is worse than it looks — it does not merely leave the two disagreeing, it
leaves *nothing for the retry to find*. A second expiry pass over an
already-cleaned buffer removes nothing, reports `Ok(0)`, and never repairs the
durable side, so the failure becomes permanent and the restart resurrects the
data. Any new operation that deletes must be able to return `Err` without
having consumed its own evidence.

**What breaks it.** Expiring from the buffer alone. Mutating memory before the
durable change it stands for. Treating any replay failure as a torn tail.
Leaving a torn tail in the file. Deriving the flush threshold from
`writer.len()` alone.

**Symptom.** Expired spans that return after a restart; acknowledged batches
that vanish silently after a bad sector; a log that grows without bound under
retries and an OOM on the restart that has to replay it; a full disk or a
permission error turning one failed maintenance pass into permanently divergent
state.

---

## 11. The server owns no storage

**The rule.** `traza-server` has no span storage of its own — no side log, no
in-memory index, no cache. Every ingest goes through `traza::Store` and every
read comes back out of it.

**Where.** `serve_request` calls only engine methods.

**Why it matters.** Restart durability is entirely the engine's, and anything
the server accepts is visible to any other engine reader of the same directory.
A server-side cache would be a second source of truth with its own crash
semantics. `tests/server_on_engine.rs` opens the server's data directory with
`traza::Store` directly and compares — that is the oracle.

---

## Rules of thumb for changing this code

- **If it is only reachable under concurrency, a single-threaded test will not
  find it.** Invariants 2, 6, and 9 all have failure modes invisible to a
  sequential test.
- **Recovery follows the journal, never the content** (invariant 5).
- **Order is the contract** — of segments (1), of locks (2), of fsync and
  acknowledgement (9).
- **A change is not done until the recovery authority agrees with memory**
  (invariant 10). Ask what a restart would say about it.
- **Never resolve a correctness question by dropping data quietly.** Every one
  of these rules has a failure mode whose only symptom is missing or resurrected
  spans, which is why several of them end in "refuse" rather than "recover".
- Before you finish, read [testing](testing.md): a test that guards one of
  these must be shown to **fail** when the behaviour is broken.
