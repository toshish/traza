# Traza Storage Benchmark

These values were measured by `cargo run --release --bin storage-bench`; they are not estimates. The benchmark starts `target/release/traza-server` on a free loopback port with a fresh temporary data directory, ingests a corpus over HTTP while counting the exact request-body bytes it sends, waits for the flush and for compaction to quiesce, then walks the data directory.

## Results

| Corpus | Spans | Ingested | On disk | Ratio (in:stored) | Amplification | Bytes/span |
|---|---:|---:|---:|---:|---:|---:|
| `generic` | 1000000 | 311.9 MiB | 564.3 MiB | 0.55 : 1 | 1.81x | 592 |
| `llm` | 200000 | 438.7 MiB | 909.5 MiB | 0.48 : 1 | 2.07x | 4768 |
| `pinned-context` | 10000 | 3144.4 MiB | 26.3 MiB | 119.72 : 1 | 0.01x | 2754 |

Where the bytes are:

| Corpus | Segment files | Write-ahead log | Payload store | Other | Total | Segment count |
|---|---:|---:|---:|---:|---:|---:|
| `generic` | 556.7 MiB | 0.0 MiB | 0.0 MiB | 7.64 MiB | 564.3 MiB | 3 |
| `llm` | 905.0 MiB | 0.0 MiB | 0.0 MiB | 4.47 MiB | 909.5 MiB | 5 |
| `pinned-context` | 25.7 MiB | 0.0 MiB | 0.3 MiB | 0.28 MiB | 26.3 MiB | 1 |

## Storage cost

Priced per GiB-month at the rates a storage comparison conventionally uses: $0.08 for block storage, $0.023 for object storage. Traza keeps its data in a local data directory, so it pays the block rate, and an HA cluster keeps one copy per node.

| Corpus | Stored | 1 node / month | 3-node HA / month |
|---|---:|---:|---:|
| `generic` | 0.55 GiB | $0.044 | $0.132 |
| `llm` | 0.89 GiB | $0.071 | $0.213 |
| `pinned-context` | 0.03 GiB | $0.002 | $0.006 |

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
- Build: Cargo release profile. Timestamp: Unix 1788161696.
- Machine context: macos/aarch64, 10 available hardware threads.
- Final server stats (`generic`): `{"buffer_age_seconds":null,"buffered_records":0,"bytes_on_disk":583740679,"durability":"wal","persisted_records":1000000,"record_count":1000000,"segment_count":3,"total_records":1000000,"wal_bytes":0}`.
- Final server stats (`llm`): `{"buffer_age_seconds":null,"buffered_records":0,"bytes_on_disk":948944130,"durability":"wal","persisted_records":200000,"record_count":200000,"segment_count":5,"total_records":200000,"wal_bytes":0}`.
- Final server stats (`pinned-context`): `{"buffer_age_seconds":null,"buffered_records":0,"bytes_on_disk":26914752,"durability":"wal","persisted_records":10000,"record_count":10000,"segment_count":1,"total_records":10000,"wal_bytes":0}`.

## Verification Notes

- Every reported byte count is measured by this run, never estimated.
- The benchmark fails rather than reports if the store does not hold exactly the corpus it ingested.
- Traza stores span payloads as JSON and does not compress them. A ratio below 1:1 is amplification, and is reported as measured rather than inverted into a flattering number.
- Exact byte counts, so anything derived from this table can be recomputed rather than re-rounded:
  - `generic`: 1000000 spans, 327069707 bytes ingested, 591748106 bytes on disk (583740679 segments, 8 write-ahead log, 0 payload store, 8007419 other).
  - `llm`: 200000 spans, 460055297 bytes ingested, 953628120 bytes on disk (948944130 segments, 8 write-ahead log, 0 payload store, 4683982 other).
  - `pinned-context`: 10000 spans, 3297157900 bytes ingested, 27540241 bytes on disk (26914752 segments, 8 write-ahead log, 327910 payload store, 297571 other).

## Records-region measurements

Measured from the kept corpora of the run above (`TRAZA_STORAGE_BENCH_KEEP=1` leaves each data directory on disk), by parsing every segment's header — records offset and length at bytes 24 and 32, per [the format document](../segment-format.md) — and decoding every record in the records region. "Value text" sums the value bytes of every record's key/value pairs; key names and length prefixes are excluded. These are the figures the [v7 format specification](../segment-format.md#format-v7-specification--ships-in-v0240) cites as its motivation.

| Corpus | Segment bytes | Records region | Share | Largest per-file share | Value text | Share of records region |
|---|---:|---:|---:|---:|---:|---:|
| `generic` | 583740679 | 522728707 | 89.6% | 89.6% | 38660000 | 7.4% |
| `llm` | 948944130 | 904194098 | 95.3% | 95.3% | 326745627 | 36.1% |
| `pinned-context` | 26914752 | 24745600 | 91.9% | 91.9% | 6337800 | 25.6% |

This section is a hand-measured addendum: `storage-bench` regenerates everything above it, so a rerun that overwrites this file re-measures these columns from its own kept corpora or removes the section rather than letting it describe a different run.
