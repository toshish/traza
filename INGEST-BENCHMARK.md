# Traza Ingest Benchmark

Every row is the MEDIAN of 1 runs, each on a fresh data directory. Payloads are generated before the clock starts, so these are server rates; client encoding is reported separately. Runs that saw a failed batch or a shed connection are reported as failures rather than as numbers.

- Machine: macos/aarch64, 10 hardware threads, Apple M1 Max
- Commit: `56b3d1f`
- Corpus: 10000 spans per run, batch 1000
- Compaction: disabled during ingest runs (a read-path optimization; its merges would steal CPU from the measurement)

| Scenario | Protocol | Keep-alive | Concurrency | Median spans/s | Min | Max |
|---|---|---|---:|---:|---:|---:|
| http-protobuf-wal-c4 | protobuf | true | 4 | **77785** | 77785 | 77785 |
