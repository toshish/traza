# Traza Query Benchmarks

These values were measured by `cargo run --release --bin query-bench`; they are not estimates. The benchmark builds and starts `target/release/traza-server` on a free loopback port with a fresh temporary data directory, ingests a corpus over concurrent HTTP clients, and then **restarts the server before every cold measurement** — the rollup cache lives in process memory, so a restart is the only way to observe a genuinely cold query.

## Results

| Query | Cold (first request after restart) | Warm p50 | Warm p95 | Cold/warm |
|---|---:|---:|---:|---:|
| stats/llm group_by=model, whole corpus | 171.2 ms | 54.2 ms | 76.0 ms | 3x |
| stats/llm group_by=model, 10% window | 604.6 ms | 438.9 ms | 494.0 ms | 1x |
| stats/llm group_by=model, 1% window | 208.4 ms | 88.4 ms | 89.5 ms | 2x |
| stats/llm group_by=session | 311.2 ms | 97.9 ms | 107.2 ms | 3x |
| sessions list | 223.8 ms | 113.5 ms | 119.8 ms | 2x |

## Methodology

- Corpus: 1000000 LLM spans, 6 models, 4 providers, 8 services, one session per 40 spans, ingested over 8 concurrent HTTP client(s) in batches of 1000. Ingest took 10.12s.
- **Concurrency is a measured axis, not a detail.** Clients take a strided slice of the corpus, so their timestamps interleave and sealed segments overlap in time. With enough concurrent clients no segment is fully inside any query window, which is exactly when the windowed aggregation path stops being able to use a cached rollup. A single-threaded ingest reports the easy case.
- Windows are absolute `since_ns`/`until_ns` bounds computed from the corpus's own time range and taken from its MIDDLE, so a 1% window really is one percent of the ingested time and is not partly answered by ruling out whole segments at the ends.
- Cold: the single first request of that shape after a server restart. Warm: 20 subsequent identical requests, nearest-rank percentiles over complete request wall-clock durations.
- Store at measurement time: 5 segments, 0 spans still in the write buffer, 0.89 GB on disk of which 33.6 MB is rollup sidecars (3.9% overhead, the price of the cold column), compaction fan-out 4 with a 268435456-byte segment ceiling. The segment count is polled until it stops moving BEFORE anything is timed, and again after every restart, so all rows describe one store shape. The buffered count matters: buffered spans have no cached rollup and are re-folded on every request, warm or cold.
- Build: Cargo release profile. Timestamp: Unix 1785399583.
- Machine context: macos/aarch64, 10 available hardware threads.

## Query paths


## Compaction churn

Every merge drops its input segments' cached rollups and publishes an output segment that has none, so the next query pays to re-establish what the merge just took away. A settled store cannot show this — by then every rollup has been rebuilt once and stays warm. These samples are `GET /v1/stats/llm?group_by=model` fired continuously FOR THE WHOLE of ingest, flush and settle, which is the only time compaction has anything to do — and the honest shape of the question, since a dashboard queries a store that is still being written to. Merges are counted as decreases in the segment count; seals increase it.

| Metric | Value |
|---|---:|
| Queries fired during ingest | 558 |
| Merge events observed | 2 |
| p50 during compaction | 57.7 ms |
| p95 during compaction | 118.8 ms |
| Worst single query | 299.0 ms |
| p50 once settled | 53.8 ms |
| Churn penalty (p95 during / p50 settled) | 2.2x |

A run that observed zero merge events proves nothing about compaction; check the merge count before reading the rest of this table.
- `/v1/stats/llm?group_by=model` — every segment is fully inside the window, so every segment can be answered from its rollup
- `/v1/stats/llm?group_by=model&since_ns=1700000449999550000&until_ns=1700000549999450000` — the dashboard case: segments straddling the window boundary are decoded rather than rolled up
- `/v1/stats/llm?group_by=model&since_ns=1700000494999505000&until_ns=1700000504999495000` — the narrow dashboard case, where the decoded fraction of each straddling segment matters most
- `/v1/stats/llm?group_by=session` — the highest-cardinality grouping, so the merge rather than the decode dominates
- `/v1/sessions?limit=50` — the same rollups behind a different projection

## Verification Notes

- Every reported result is measured by this benchmark run, never estimated.
- A non-200 response aborts the run rather than being recorded as a fast query.
- The cold column cannot be re-measured without another restart; it is one sample by construction, so read it as an order of magnitude and not as a percentile.
