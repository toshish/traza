# Architecture

Two layers with one contract: never lose a completed write, never serve a torn
one.

```
         HTTP/1.1 (std::net)                 traza::Store
    ┌───────────────────────────┐      ┌──────────────────────────────┐
    │  bounded connection pool  │      │  write buffer (in memory)    │
    │  request framing          │─────▶│  write-ahead log (wal.log)   │
    │  auth gate                │      │  sorted immutable segments   │
    │  route dispatch           │◀─────│  payload store, annotations  │
    │  static dashboard files   │      │  indexes, rollup cache       │
    └───────────────────────────┘      └──────────────────────────────┘
       src/bin/traza-server.rs              src/lib.rs + modules
```

## The storage engine

`traza::Store` (in [`src/lib.rs`](../../src/lib.rs)) is the whole datastore. It
is a library first: it embeds in any process, and the server is one of its
callers rather than a layer above it.

Spans are buffered in memory keyed by their `(trace_id, span_id)` primary key,
appended to a write-ahead log, and periodically sealed into sorted, immutable
segment files. Reads combine the buffer and the segments under one atomic
snapshot.

A segment is one file — a fixed header, the encoded records in ascending
timestamp order, then a record-offset index, a trace index, and an attribute
index. Opening a segment parses **only the indexes**; record payloads stay on
disk and are read by exact byte range on demand. That is what lets a store
larger than RAM serve correctly. Byte layout: [segment format](../segment-format.md).

Filters narrow candidates through those indexes and then **re-verify every
predicate against the parsed span**. An index accelerates a filter; it never
changes its semantics.

## The HTTP server

[`src/bin/traza-server.rs`](../../src/bin/traza-server.rs) is a deliberately
small HTTP/1.1 implementation on `std::net`. It owns **no span storage** — no
side log, no in-memory index. Anything it accepts is durable under the engine's
rules and visible to any other engine reader of the same directory.

Concurrency is bounded by connections, not by a queue. Keep-alive means a
persistent connection occupies its handler until the client is done with it, so
queueing past the limit would leave clients waiting indefinitely; past
`--max-connections` a client is refused immediately with `503`.

Because connections persist, request framing is security-relevant: anything
ambiguous about where a body ends would let one request be split into two, with
the remainder attributed to the client's next request. Transfer-encoded bodies
and duplicate `Content-Length` headers are therefore **refused rather than
resolved**, and any response sent without reading the request's body closes the
connection.

The server also serves the dashboard's build directory as static files,
resolved against the canonicalized root so no request can read outside it.

## The write path, socket to sealed segment

What happens between `POST /v1/spans` and bytes on disk.

### 1. Read the head, gate, then read the body

The request head is parsed first. **The auth verdict is reached before the body
is read** — otherwise an unauthenticated client could make the server buffer
64 MiB just by declaring a `Content-Length`. A rejected request closes the
connection, precisely because its body was never consumed.

### 2. Decode

`POST /v1/spans` deserializes straight into `Vec<Span>` — not via
`serde_json::Value`. `POST /v1/traces` decodes protobuf or JSON into the OTLP
JSON shape and maps that to spans. Either way the stage is timed as
`traza_http_decode_ns_*`.

### 3. Validate the primary key

Both halves of `(trace_id, span_id)` must be non-empty, checked at the HTTP
surface *and* again at the engine boundary in `validate_span`. Distinct spans
sharing an empty id would silently collapse into one upserted key, and the
response would still count them all as accepted. Validation is atomic per
batch.

### 4. Offload oversized payloads

String attribute values above `payload_threshold` are extracted to the
content-addressed payload store and replaced in the span by a `$payload`
reference. This happens before the span reaches the log, so the log never
carries a multi-megabyte prompt.

### 5. `Store::admit` — the acknowledgement path

Ordering here *is* the durability contract:

1. **Encode the log frame before taking the writer lock.** Serializing a batch
   is pure CPU proportional to its size; doing it under the lock made every
   concurrent ingest wait for one thread's JSON encoding.
2. **Take the writer lock.** Append the frame to `wal.log` and upsert the batch
   into the write buffer, both under the lock, so a concurrent flush cannot
   seal a buffer that disagrees with the log.
3. **Release the lock.**
4. **fsync, then return.**
5. **Seal, if the buffer has reached one of its bounds** (below) — outside the
   lock, and outside step 2's lock on purpose.

The fsync is deliberately outside the lock. That is what lets concurrent
batches accumulate into a single sync — group commit — instead of serializing
one fsync per request. A waiter wakes when *some* sync has covered its log
sequence number.

A crash before step 4 loses the batch, which is correct: nothing had been
acknowledged. The response is sent only after the fsync it promises.
`Durability::Flushed` is the exception to the ordering: it promises a *sealed
segment* rather than an fsync, so it seals before returning and the segment
supersedes the fsync it would otherwise have waited for.

### 6. Sealing a segment

A seal has the same three-phase shape as compaction and expiry, and for the
same reason — the I/O is the expensive part and it needs no lock:

1. **Drain, under a short writer lock.** The buffered spans are copied out as
   `Arc<Span>` handles and the next segment id is claimed. Both cheap.
2. **Write, with no engine lock held.** Sort into Traza's stable order, then
   **encode → write to a temp file → `fsync` the file → atomic `rename` →
   `fsync` the directory**. The segment is then **reopened file-backed**, so
   the encoded buffer is dropped and it serves reads from disk immediately —
   sealing never leaves a resident payload copy behind. This is the part that
   used to hold the writer lock, and it was ~74% of everything that lock was
   held for.
3. **Publish and reconcile, under short locks.** The segment is pushed onto
   the list; then the log is reclaimed and the sealed spans are dropped from
   the buffer.

Three details carry the correctness:

- **The drain copies; it does not remove.** Taking the spans out at step 1
  would leave already-acknowledged data in neither the buffer nor a segment for
  the whole of step 2 — briefly invisible to readers. A merge keeps its inputs
  live and pinned until its output is published; a seal does the same, which is
  why no reader needs to know a seal is happening.
- **Eviction is by handle identity, not by key.** A span re-ingested during the
  write is a newer version living in the buffer while the segment holds the
  older one. The buffer outranks segments, so reads are correct — but the
  eviction must drop only keys whose current value is still the sealed one.
  `Arc::ptr_eq` answers that; comparing values cannot, because a span
  re-ingested unchanged is a newer version that happens to look identical.
- **The segment id is claimed at the drain.** Segment path order is recency
  order, so an id claimed at completion could invert last-write-wins.
  Compaction claims from the same counter and therefore declines to claim one
  while a seal holds a lower unpublished id.

One seal runs at a time, behind a permit; a second would drain the same spans
the first already covers. Expiry takes the permit too, so a seal that drained
before a deletion cannot publish its segment afterwards and resurrect what was
deleted. Ingest never takes it while holding the writer lock — that would
invert the lock order.

If the write fails, nothing was published and nothing was removed: the buffer
and the log are exactly as they were, and the next seal retries.

## The read path

`query` takes the writer lock and then the segments lock — always in that order
— and resolves the whole answer under that one snapshot. A concurrent seal can
therefore neither hide a committed span (its spans stay in the buffer until the
segment is published) nor duplicate one (the buffer wins on the primary key
while both copies exist).

Precedence follows the primary key: the write buffer holds the newest version
of anything it carries and always wins; among segments, a later path is a later
flush and therefore newer.

Two strategies share that snapshot:

- **Limited queries** keep per-source candidates as posting/record offsets and
  run a k-way merge that decodes one head per source. Only the selected source
  advances, so each record is read at most once. Heads are compared with the
  full `(start, end, trace, span)` order — comparing only the timestamp made
  equal-time ties depend on source order, and cursor consumers such as export
  then skipped valid rows.
- **Unlimited queries** narrow candidates through the same indexes. A candidate
  is emitted only if no higher-precedence source also holds its key, which is
  an index lookup rather than a decode.

`query_after` adds an exclusive cursor in that same total order, which is the
bounded pagination primitive `GET /v1/export` streams with.

A lock cannot span pages, so a multi-page read needs more than a per-call
snapshot: `Store::snapshot` copies the write buffer and clones the segment
`Arc`s into a `SnapshotView` that answers from that one instant however the
store changes afterwards. Segments are reference-counted and hold their own
descriptors, so compaction and expiry unlink files a view is still reading and
the space comes back when it drops. Export pages a view, which is what lets its
`complete` trailer mean "this is the whole dataset" rather than "the connection
ended tidily".

## Startup and recovery

`Store::open`, in order:

1. Create the directory if absent, then **acquire the directory lock**. One
   live `Store` per directory. A lock naming a PID that verifiably no longer
   exists is stale and reclaimed by exactly one winner through a sentinel file;
   an unreadable or live lock rejects the open.
2. **Remove orphaned temp files** left by an interrupted segment write.
3. **Finish interrupted compaction rewrites** from the supersede journal.
   Recovery follows the journal, never content.
4. **Load segments** and sort them by path — which is recency order.
5. **Recover the write-ahead log** into the buffer, before accepting any new
   write. Records are append-ordered and upserted in that order, so the newest
   version of a re-ingested key wins exactly as it did before the crash. A torn
   trailing record is discarded — it was never acknowledged — and the file is
   truncated to the last complete frame. Damage in a frame that *did* complete
   fails the open instead: frames after it may be acknowledged batches, and
   returning the prefix would drop them silently.
6. Open the annotation log, replaying it into its in-memory index.

A `buffered` store neither writes nor reads a log, and leaves any existing log
untouched — restarting in `wal` mode must still recover it.

## Background maintenance

The server starts one maintenance thread when TTL or compaction is enabled. It
ticks every 5 seconds and runs `compact_segments`; every twelfth tick (one
minute) it also runs `compact_expired` if TTL is configured.

**Compaction** merges same-size segments to bound the segment count, because a
filtered query costs one index probe per segment — a store that only ever
appends flush-sized segments gets steadily slower to search. Only the **tail**
of the segment list is merged, for the reason in
[invariants](invariants.md#1-segment-path-order-is-recency-order).

**TTL expiry** removes spans, annotations, and payload files older than the
retention window, and rewrites the write-ahead log to the spans that survived —
otherwise a restart replays what was just deleted. Live payload references are
computed *after* span expiry, so payloads referenced only by just-expired spans
become sweepable.

Both are **rewriting** operations: they take the maintenance lock (so only one
runs at a time), pin the segments they will replace, do every byte of parsing,
writing and fsyncing with no engine lock held, and take the segment lock back
only to publish — revalidating first that what they pinned is still there.
Reads and ingest therefore never wait on a merge or an expiry pass. This is not
a nicety: holding the segment lock across a merge made queries wait for it, and
a waiting query holds the writer lock, so ingest queued behind the query. A
large merge was an outage. What this does *not* claim is that maintenance is
free — it competes for CPU and disk like any other work, and that contention is
unmeasured.

## Satellite stores

Two record types live outside segments, because their volume and access shape
differ by orders of magnitude:

- **Annotations** — an append-only JSONL log fsynced per append, with an
  in-memory index by trace. Human/eval scale, so a flat log is the honest
  design. The TTL compactor rewrites it dropping expired entries.
- **Payloads** — content-addressed files at `payloads/<aa>/<sha256>.bin`,
  immutable once written. Identical content is stored once.

## Further reading

- [Invariants](invariants.md) — what a change must not break
- [Module map](module-map.md) — file-by-file responsibility
- [Segment format](../segment-format.md) — the on-disk bytes
- [Durability](../operations/durability.md) — the operator-facing contract
