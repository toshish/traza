# Traza Benchmarks

These values were measured by `cargo run --release --bin bench`; they are not estimates. The benchmark builds and starts `target/release/traza-server` on a free loopback port with a fresh temporary data directory.

## Results

| Metric | Measured | Target | Result |
|---|---:|---:|---|
| Sustained batched HTTP ingest (durability=wal, compaction fanout=4, max segment bytes=268435456) | 52569 spans/s | >= 50,000 spans/s | PASS |
| Trace-by-id p95 | 0.926 ms | < 50 ms | PASS |
| Attribute-filtered query p95 | 2.873 ms | < 300 ms | PASS |

Additional percentiles:

| Query | p50 | p95 | p99 | samples |
|---|---:|---:|---:|---:|
| Trace by ID | 0.393 ms | 0.926 ms | 1.553 ms | 200 |
| Attribute filter | 1.695 ms | 2.873 ms | 5.521 ms | 100 |

## Methodology

- Corpus: 1000000 spans, 100,000 traces with 10 spans each, 20 services, 100 indexed `benchmark.group` attribute values, and occasional events.
- Ingest: HTTP `POST /v1/spans`, 1000 spans per request, timed from the first request through the final successful response. JSON generation is intentionally inside the timed loop, so the reported rate includes client serialization and loopback HTTP overhead.
- Trace sampling: 200 deterministic trace IDs spread through the corpus; each response is parsed and checked for 10 spans.
- Filter sampling: 100 deterministic `attr.benchmark.group` queries with `limit=100`; each response body is parsed as JSON.
- Percentiles: nearest-rank selection over complete request wall-clock durations measured with `std::time::Instant`; no warm-up samples are discarded.
- Build: Cargo release profile. Timestamp: Unix 1786832119.
- Machine context: macos/aarch64, 10 available hardware threads.
- Final server stats: `{"buffer_age_seconds":null,"buffered_records":0,"bytes_on_disk":583863347,"durability":"wal","persisted_records":1000000,"record_count":1000000,"segment_count":36,"total_records":1000000,"wal_bytes":0}`.

The ingest threshold is PASS. The trace p95 threshold is PASS. The filtered-query p95 threshold is PASS. Any miss remains visible in the table rather than being substituted or estimated.

## Verification Notes

- Corpus declaration: `1000000` spans (1,000,000 spans).
- Every reported result is measured by this benchmark run, never estimated.
- Unsuccessful lookups are reported as misses.
