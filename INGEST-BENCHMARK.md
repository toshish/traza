# Traza Ingest Benchmark

Every row is the MEDIAN of 5 runs, each on a fresh data directory. Payloads are generated before the clock starts, so these are server rates; client encoding is reported separately. Runs that saw a failed batch or a shed connection are reported as failures rather than as numbers.

- Machine: macos/aarch64, 10 hardware threads, Apple M1 Max
- Commit: `96604cb`
- Corpus: 200000 spans per run, batch 20
- Compaction: disabled during ingest runs (a read-path optimization; its merges would steal CPU from the measurement)

| Scenario | Protocol | Keep-alive | Concurrency | Median spans/s | Min | Max |
|---|---|---|---:|---:|---:|---:|
| http-json-wal-keepalive-off | json | false | 8 | **9203** | 8908 | 9273 |
| http-json-wal-keepalive-on | json | true | 8 | **10221** | 9969 | 10470 |
