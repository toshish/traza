# Needle demo

A million spans through the front door, then find one sentence.

```sh
examples/needle/run.sh
```

It starts `traza-server --durability wal --profile throughput` (the documented
bulk-backfill profile, [configuration](../../docs/configuration.md#profiles))
on a throwaway store, floods it with ~1,000,000 lean spans over plain HTTP
`POST /v1/spans` — six services, timestamps spread across the last 30 days,
exactly one needle span hidden mid-flood — and then runs six timed queries:
three hunt the needle (trace lookup, attribute filter, content search), and
three measure the store around it (a word it has never seen, a time window,
a whole-corpus aggregate). The store is deleted on exit.
`python3` (stdlib only) is the load generator and the stopwatch; `curl` does
the rest.

## What it shows

That a store you filled ten seconds ago answers point queries in milliseconds,
and that the engine will show its work: every `/v1/spans` response carries
`cost: {elapsed_ns, segments_examined, segments_pruned}`, so "the index made
this cheap" is a number in the output rather than a claim in a README.

## The beats

| Beat | What to watch for |
|---|---|
| flood | The acknowledged count is a sum of server responses, each batch checked against its size. The spans/s figure measures this Python client as much as the server. |
| trace lookup | `GET /v1/traces/needle-trace-1`, 200 round-trips on one keep-alive connection → client p50/p95. |
| attribute filter | `attr.needle=true` against the whole store — asserted to match **exactly one** span, with the cost object printed. |
| content search | `q=aubergine midnight` — two words no other span contains. The content index prunes every segment but the needle's, and the engine time is printed separately from the HTTP round-trip. |
| absent word | `q=xylotheque` — every segment pruned without being read. The run prints the engine's own figure — microseconds on the machines this was written on; the asserted bound is a generous 250 ms. |
| time window | last 2 days + a service filter — "K of M segments pruned by the time range" read off the cost object, which is the point of spreading timestamps across 30 days. |
| aggregate | `GET /v1/stats/duration` over the whole corpus, asserted to have folded exactly as many spans as were acknowledged. |
| store | `GET /v1/stats` — bytes on disk, segment count, and a record count asserted equal to the number of spans acknowledged. |

At the end it prints the dashboard deep link for the same content search
(`#/traces?c=aubergine%20midnight`), where the query-cost line renders under
the results. The dashboard has to be built once —
`cd ui && npm ci && npm run build` — or the link answers a JSON 404; the
server picks up the build without a restart, and the run says which case
it found.

## Knobs

| Variable | Default | Meaning |
|---|---|---|
| `TRAZA_DEMO_PORT` | `8126` | Server port |
| `TRAZA_NEEDLE_SPANS` | `1000000` | Spans to flood (CI smoke uses `60000`) |
| `TRAZA_NEEDLE_HOLD` | unset | `1` pauses before cleanup so the dashboard link works, and also pauses 6 s before the flood so you can open `#/overview` and watch the ingest sparkline take it — build the dashboard first (above) |

## Honest caveats

- **Segment records are LZ4-compressed; the indexes beside them are stored
  raw** — the trade behind the latencies above, spelled out in
  [storage-comparison](../../docs/storage-comparison.md). Measured on this
  corpus: ~123 MiB on disk for 1,000,000 lean spans (~7 MiB at the 60,000
  smoke scale). Budget ~0.5 GiB free disk for a full run — the store passes
  through more than its settled size while compaction is still merging — and
  fatter spans cost proportionally more.
- **The ingest rate is client-bound.** A python3/`http.client` flood from the
  same laptop measured ~194,000 spans/s here; the server's own benchmark rig
  has measured 250,453 spans/s, on a pre-v7 build at v0.16
  (`docs/benchmarks/ingest.md`). The demo prints whatever it measured, never
  a quoted number.
- **`wal` durability on macOS survives `kill -9`, not power loss** — macOS
  `fsync` does not reach the platter without `F_FULLFSYNC`
  ([durability](../../docs/operations/durability.md)).
- Latency assertions are generous sanity bounds (e.g. lookup p95 < 250 ms),
  meant to catch pathology in CI without flaking on machine variance. The
  interesting numbers are the printed ones.

## Runtime

Measured on an Apple-silicon laptop: the full run finishes in ~10 s (flood
~5 s, aggregate ~3 s); the 60,000-span smoke run finishes in under 2 s.
