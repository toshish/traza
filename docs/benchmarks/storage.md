# Traza Storage Benchmark

These values were measured by `cargo run --release --bin storage-bench`; they are not estimates. The benchmark starts `target/release/traza-server` on a free loopback port with a fresh temporary data directory, ingests a corpus over HTTP while counting the exact request-body bytes it sends, waits for the flush and for compaction to quiesce, then walks the data directory.

## Results

| Corpus | Spans | Ingested | On disk | Ratio (in:stored) | Amplification | Bytes/span |
|---|---:|---:|---:|---:|---:|---:|
| `generic` | 1000000 | 311.9 MiB | 126.5 MiB | 2.47 : 1 | 0.41x | 133 |
| `llm` | 200000 | 438.7 MiB | 102.8 MiB | 4.27 : 1 | 0.23x | 539 |
| `pinned-context` | 10000 | 3144.4 MiB | 4.1 MiB | 769.74 : 1 | 0.00x | 428 |

Where the bytes are:

| Corpus | Segment files | Write-ahead log | Payload store | Other | Total | Segment count |
|---|---:|---:|---:|---:|---:|---:|
| `generic` | 118.9 MiB | 0.0 MiB | 0.0 MiB | 7.64 MiB | 126.5 MiB | 3 |
| `llm` | 98.4 MiB | 0.0 MiB | 0.0 MiB | 4.46 MiB | 102.8 MiB | 2 |
| `pinned-context` | 3.8 MiB | 0.0 MiB | 0.0 MiB | 0.28 MiB | 4.1 MiB | 1 |

## Storage cost

Priced per GiB-month at the rates a storage comparison conventionally uses: $0.08 for block storage, $0.023 for object storage. Traza keeps its data in a local data directory, so it pays the block rate, and an HA cluster keeps one copy per node.

| Corpus | Stored | 1 node / month | 3-node HA / month |
|---|---:|---:|---:|
| `generic` | 0.12 GiB | $0.010 | $0.030 |
| `llm` | 0.10 GiB | $0.008 | $0.024 |
| `pinned-context` | 0.00 GiB | $0.000 | $0.001 |

For reference, the same stored volume on object storage at $0.023/GiB-month would cost 3.5x less. Traza has no object-storage tier; the rate is quoted only so the gap is legible.

## Methodology

- **`generic`** — Service traces: 10-span traces across 20 services, three indexed attributes, occasional events. The same span shape the throughput benchmark uses.
- **`llm`** — LLM calls: OpenLLMetry `gen_ai.*` / `traceloop.*` attributes with a shared system prompt, a per-call user prompt, and a completion — roughly 2 KiB of text per span. Every value is below the payload-offload threshold, so nothing is deduplicated.
- **`pinned-context`** — Long-context agent calls: the same LLM span carrying a 320 KiB pinned context that is byte-identical on every call, above the 256 KiB payload threshold. This is the case content-addressed offloading exists for — the context is stored once for the whole corpus.
- Ingest for `generic`: HTTP `POST /v1/spans`, 1000 spans per request.
- Ingest for `llm`: HTTP `POST /v1/spans`, 1000 spans per request.
- Ingest for `pinned-context`: HTTP `POST /v1/spans`, 100 spans per request.
- "Ingested" is the sum of request-body lengths actually written to the socket — the JSON a client would have sent to any other backend — excluding HTTP headers.
- "On disk" is a recursive walk of the whole data directory: segments, write-ahead log, payload store, and everything else. It is not the `bytes_on_disk` field of `/v1/stats`, which counts segments only.
- Quiescence: the benchmark forces a flush, then polls `/v1/stats` until the segment count and disk usage stop changing, so compaction is not caught mid-rewrite.
- Configuration: shipped defaults throughout — `--durability wal`, compaction on, payload threshold at its default. No setting was tuned for this measurement.
- Build: Cargo release profile. Timestamp: Unix 1788180702.
- Machine context: macos/aarch64, 10 available hardware threads.
- Final server stats (`generic`): `{"buffer_age_seconds":null,"buffered_records":0,"bytes_on_disk":124662599,"durability":"wal","persisted_records":1000000,"record_count":1000000,"segment_count":3,"total_records":1000000,"wal_bytes":0}`.
- Final server stats (`llm`): `{"buffer_age_seconds":null,"buffered_records":0,"bytes_on_disk":103133975,"durability":"wal","persisted_records":200000,"record_count":200000,"segment_count":2,"total_records":200000,"wal_bytes":0}`.
- Final server stats (`pinned-context`): `{"buffer_age_seconds":null,"buffered_records":0,"bytes_on_disk":3979924,"durability":"wal","persisted_records":10000,"record_count":10000,"segment_count":1,"total_records":10000,"wal_bytes":0}`.

## Verification Notes

- Every reported byte count is measured by this run, never estimated.
- The benchmark fails rather than reports if the store does not hold exactly the corpus it ingested.
- **This file exists only because the run passed acceptance gate 5** of [the segment format](../segment-format.md#acceptance-gates): settled amplification at or below 1.0x on `generic` and `llm`. The benchmark asserts the gate after measuring and before writing; a run that misses it exits non-zero and writes nothing.
- Segment records and payload blobs are LZ4-compressed (format v7). A ratio below 1:1 would be amplification and would be reported as measured rather than inverted into a flattering number.
- Exact byte counts, so anything derived from this table can be recomputed rather than re-rounded:
  - `generic`: 1000000 spans, 327069707 bytes ingested, 132670049 bytes on disk (124662599 segments, 8 write-ahead log, 0 payload store, 8007442 other).
  - `llm`: 200000 spans, 460055297 bytes ingested, 107814355 bytes on disk (103133975 segments, 8 write-ahead log, 0 payload store, 4680372 other).
  - `pinned-context`: 10000 spans, 3297157900 bytes ingested, 4283487 bytes on disk (3979924 segments, 8 write-ahead log, 5961 payload store, 297594 other).

## Records-region measurements

Measured by this same run, from the v7 header of every segment file the settled store holds: the header is 128 bytes, the records region's STORED (compressed) length is the u64 at byte 32, and its LOGICAL (uncompressed) length is the u64 at byte 120 — the header table in [the format document](../segment-format.md#the-v7-header) is the reference. The records region is LZ4-compressed and addressed through the block directory, so the logical column is the byte count the directory's blocks decode to, not anything present contiguously in the file.

| Corpus | Segment bytes | Records stored | Stored share | Largest per-file share | Records logical | Stored/logical |
|---|---:|---:|---:|---:|---:|---:|
| `generic` | 124662599 | 63530203 | 51.0% | 51.0% | 492068707 | 0.13 |
| `llm` | 103133975 | 58384423 | 56.6% | 56.6% | 544648471 | 0.11 |
| `pinned-context` | 3979924 | 1806620 | 45.4% | 45.4% | 16767800 | 0.11 |

The v6-era edition of this section also measured a "value text" column: the share of the records region that was attribute value text stored twice, once in the payload and once in the record's own key/value list. v7 removed that double-store — records carry fixed-width `(key id, digest)` pairs, and the value text lives only in the payload — so the column no longer measures anything. The v6-era numbers (36.1% of `llm`'s records region, 25.6% of `pinned-context`'s, 7.4% of `generic`'s, at `65652a2`) remain quoted in [the format document's motivation](../segment-format.md#format-v7) as the measurement that justified the change, and in this file's git history.
