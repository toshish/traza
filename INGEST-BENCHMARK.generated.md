Every row is the MEDIAN of 5 runs, each on a fresh data directory. Scenarios are run ROUND-ROBIN rather than one at a time, and their order is ROTATED each round, so each scenario's repeats are spread across the whole wall-clock window and across positions within a round. Background load then hits all of them alike instead of landing on whichever ran during a spike or whichever is pinned to the same phase of a periodic load. Payloads are generated before the clock starts, so these are server rates; client encoding is reported separately. Runs that saw a failed batch or a shed connection are reported as failures rather than as numbers.

- Machine: macos/aarch64, 10 hardware threads, Apple M1 Max
- Commit: `985d236`
- Corpus: 600000 spans per run, batch 1000
- Compaction: disabled during ingest runs (a read-path optimization; its merges would steal CPU from the measurement)

Latency is the CLIENT-OBSERVED time for one acknowledged batch, sampled per request and reduced to percentiles per run; the table reports the MEDIAN ACROSS RUNS of each percentile. Read it with the load model in mind: this is a closed-loop generator with a fixed number of workers, all saturating, so latency includes queueing and by Little's law tracks concurrency divided by throughput. Latencies are therefore only comparable BETWEEN ROWS AT THE SAME CONCURRENCY, and the honest place to look for a deliberate delay's cost is the low-concurrency rows, where there is nothing to queue behind.

| Scenario | Protocol | Keep-alive | Concurrency | Median spans/s | Min | Max | p50 ms | p95 ms | p99 ms |
|---|---|---|---:|---:|---:|---:|---:|---:|---:|
| variant-flush-spans-1000-c8 | json | true | 8 | **36857** | 34270 | 47715 | 213.48 | 248.01 | 259.68 |
| variant-flush-spans-1000-openloop-c8 | json | true | 8 | FAILED: offered rate exceeded capacity: delivered 37389 of 60000 spans/s (worst lateness 5839.2 ms), so this is a saturation measurement, not an open-loop one | | | | | |
| variant-flush-spans-2000-c8 | json | true | 8 | **57574** | 51035 | 65681 | 138.29 | 158.01 | 164.08 |
| variant-flush-spans-2000-openloop-c8 | json | true | 8 | FAILED: offered rate exceeded capacity: delivered 52839 of 60000 spans/s (worst lateness 1211.5 ms), so this is a saturation measurement, not an open-loop one | | | | | |
| variant-flush-spans-3000-c8 | json | true | 8 | **74077** | 66940 | 79731 | 108.57 | 131.90 | 145.09 |
| variant-flush-spans-3000-openloop-c8 | json | true | 8 | **59834** | 59815 | 59889 | 33.36 | 48.43 | 56.43 |
| variant-flush-spans-5000-c8 | json | true | 8 | **97857** | 92938 | 110838 | 81.24 | 108.09 | 114.08 |
| variant-flush-spans-5000-openloop-c8 | json | true | 8 | **59845** | 59778 | 59907 | 27.44 | 47.45 | 52.56 |
| variant-flush-spans-10000-c8 | json | true | 8 | **137859** | 133755 | 147249 | 62.46 | 80.06 | 91.31 |
| variant-flush-spans-10000-openloop-c8 | json | true | 8 | **59779** | 59654 | 59842 | 18.40 | 55.99 | 64.00 |
| variant-flush-spans-20000-c8 | json | true | 8 | **158409** | 104522 | 210150 | 34.53 | 96.25 | 102.13 |
| variant-flush-spans-20000-openloop-c8 | json | true | 8 | **59672** | 59573 | 59726 | 17.93 | 69.73 | 80.33 |
| variant-flush-spans-30000-c8 | json | true | 8 | **165329** | 149997 | 225258 | 28.73 | 116.32 | 127.33 |
| variant-flush-spans-30000-openloop-c8 | json | true | 8 | **59572** | 59510 | 59599 | 15.66 | 84.04 | 98.22 |
