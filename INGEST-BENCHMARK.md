# Traza Ingest Benchmark

Every row is the MEDIAN of 1 runs, each on a fresh data directory. Payloads are generated before the clock starts, so these are server rates; client encoding is reported separately. Runs that saw a failed batch or a shed connection are reported as failures rather than as numbers.

- Machine: macos/aarch64, 10 hardware threads, Apple M1 Max
- Commit: `1d89d11`
- Corpus: 2000 spans per run, batch 1000
- Compaction: disabled during ingest runs (a read-path optimization; its merges would steal CPU from the measurement)

| Scenario | Protocol | Route | Keep-alive | Concurrency | Median spans/s | Min | Max | Bytes/span | Decode ns/span |
|---|---|---|---|---:|---:|---:|---:|---:|---:|
| http-native-json-wal-keepalive-off | native-json | /v1/spans | false | 8 | **117822** | 117822 | 117822 | 257 | 1643 |
| http-native-json-wal-keepalive-on | native-json | /v1/spans | true | 8 | **125549** | 125549 | 125549 | 257 | 1239 |
| http-native-json-wal-c1 | native-json | /v1/spans | true | 1 | **107023** | 107023 | 107023 | 257 | 707 |
| http-otlp-json-wal-c1 | otlp-json | /v1/traces | true | 1 | **89276** | 89276 | 89276 | 342 | 2676 |
| http-otlp-protobuf-wal-c1 | otlp-protobuf | /v1/traces | true | 1 | **75099** | 75099 | 75099 | 117 | 4633 |
