# Traza Ingest Benchmark

Every row is the MEDIAN of 2 runs, each on a fresh data directory. Scenarios are run ROUND-ROBIN rather than one at a time, so each scenario's repeats are spread across the whole wall-clock window and background load hits all of them alike instead of landing on whichever ran during a spike. Payloads are generated before the clock starts, so these are server rates; client encoding is reported separately. Runs that saw a failed batch or a shed connection are reported as failures rather than as numbers.

- Machine: macos/aarch64, 10 hardware threads, Apple M1 Max
- Commit: `ce7dd09`
- Corpus: 60000 spans per run, batch 1000
- Compaction: disabled during ingest runs (a read-path optimization; its merges would steal CPU from the measurement)

Latency is the CLIENT-OBSERVED time for one acknowledged batch, sampled per request and reduced to percentiles per run; the table reports the MEDIAN ACROSS RUNS of each percentile. Read it with the load model in mind: this is a closed-loop generator with a fixed number of workers, all saturating, so latency includes queueing and by Little's law tracks concurrency divided by throughput. Latencies are therefore only comparable BETWEEN ROWS AT THE SAME CONCURRENCY, and the honest place to look for a deliberate delay's cost is the low-concurrency rows, where there is nothing to queue behind.

| Scenario | Protocol | Keep-alive | Concurrency | Median spans/s | Min | Max | p50 ms | p95 ms | p99 ms |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|
| profile-throughput-c4 | json | true | 4 | **158470** | 154946 | 161993 | 13.03 | 100.67 | 114.26 |
| profile-balanced-c4 | json | true | 4 | **139129** | 137724 | 140533 | 17.70 | 54.68 | 58.12 |
| profile-latency-c4 | json | true | 4 | **103964** | 99914 | 108013 | 36.01 | 49.29 | 54.08 |
