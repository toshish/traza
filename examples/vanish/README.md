# Vanish demo

Deletion with a receipt, across a tenant boundary, using nothing but `curl`
and a little `python3`.

```sh
examples/vanish/run.sh
```

It builds `traza-server` and `seed` if they are missing, starts a server with
three bearer tokens — an unbound `admin` plus `rw@acme` and `rw@zenith` — and
seeds **both tenants with identical corpora**: the same `--seed` value, so the
same trace ids, span ids, and session ids exist on both sides of the boundary.
The tenant lives in the primary key `(tenant, trace_id, span_id)` and nowhere
else, which is exactly what the demo then leans on. The store is a `mktemp`
directory, deleted on exit.

## What it shows

One tenant's user demands deletion. The demo erases that user's session for
tenant `acme` and proves four things about it, each asserted against live
responses:

1. it is **tenant-precise** — zenith's copy of the same session id, the same
   trace ids, even the same payload bytes, is untouched;
2. it is **replay-proof** — a covered span re-POSTed while the erasure is
   pending is acknowledged and deliberately not stored, and the store counts
   the suppression;
3. it reaches **every domain** — spans (superseded versions included), the
   annotation on the session, and the offloaded transcript payload, which is
   *retained by name* because zenith's identical spans still reference the
   content-addressed bytes;
4. its **receipt is a verification, not a claim** — `GET
   /v1/erasures/{id}/verify` re-walks every place the bytes could be, and
   refuses to report `erased` while a pre-erasure backup pin still holds them
   in its hard-link farm. Release the pin, and the receipt turns `erased` and
   `conclusive` — live, and again offline with
   `traza-server verify --erasure <id> --data-dir <dir>` after the server is
   stopped (exit 0).

## The beats

| Beat | What to watch for |
|---|---|
| 1 · identity in the key | Both tenants read the same trace id and get their own copies. Then the fences, verbatim: naming a foreign tenant is `403` with a reason; reading a trace only the other tenant wrote is `404`, never `403` (existence must not leak); a bound token on `/v1/stats` is `403` (even volumes disclose co-tenants). |
| 2 · the ledger | `GET /v1/tenants`: per-tenant usage, and the shared transcript blob counted against **both** quotas — each tenant's `payload_bytes_approx` must cover the blob's size, asserted. |
| 3 · the pin | `POST /v1/backups/pre-erasure`, verified before it reports success. Pins survive compaction on purpose; beat 7 turns that virtue into the finding. The backup mechanism itself — pin, copy, damage, restore — is demonstrated in [examples/crash](../crash/) (beats 5–7). |
| 4 · the request | `POST /v1/erasures` for the session, scoped to `tenant: acme`. The `200` returns only after the purge settles; the settle block's counts are printed — `spans_removed` counts physical versions, so it can exceed the session's visible span count. |
| 5 · tenant precision | The same session id: intact for zenith (span count unchanged, asserted), `404` and zero spans for acme. |
| 6 · replay-proof | The re-POST captured during beat 4's pending window: `{"accepted":0,…,"suppressed":1}`, `traza_erasure_spans_suppressed_total` incremented to match, and the span not readable afterwards. |
| 7 · the receipt | Domain-by-domain verification. First pass: `pins holds-data`, naming the `pre-erasure` pin, and `result: incomplete` — the receipt will not say "gone" while a pinned backup holds the bytes. After the release: `result: erased · conclusive: true`. |
| 8 · epilogue | The server is stopped and the same receipt runs offline against the bare data directory. Exit code `0` = erased and conclusive. |

## Knobs

| Variable | Default | Meaning |
|---|---|---|
| `TRAZA_DEMO_PORT` | `8128` | Server port |
| `TRAZA_VANISH_SCALE` | `2` | Seed scale per tenant. `1` is the CI/smoke setting; every beat still runs |

## Honest caveats

- **The replay is raced, not simulated.** Suppression exists only while the
  erasure is pending, and `POST /v1/erasures` blocks until it settles — so the
  demo re-POSTs the covered span from a concurrent loop and stops at the first
  suppressed acknowledgement. The pending window measured here is ~0.1 s and
  the loop lands inside it on the first or second attempt; if it ever missed,
  the script fails loudly rather than pretending. A replay arriving *after*
  settle is new data by design (a tombstone is a barrier, not a ban) — the
  receipt would then name it as a re-delivery and fail, which is the honest
  outcome.
- **The retained payload is correct behavior, stated in the receipt.** The
  transcript's bytes survive because zenith's spans still reference them;
  content addressing shares bytes, and reference-aware deletion keeps them
  with the reason printed. Erasing zenith's copy too would sweep the file.
- **Promotion copies would outlive this erasure, by design.** If a span of the
  session had been promoted into an eval dataset, the receipt's
  `eval-records` domain would report the copy as `attention` and stay
  inconclusive until it was tombstoned deliberately. The demo's corpus has no
  datasets, so the domain verifies `clear`; the behavior is documented in
  [administration § Erasure](../../docs/operations/administration.md#erasure-deletion-with-a-receipt),
  not exercised here.
- **Retention is the erasure's quieter sibling.** Per-tenant TTL
  (`--tenant-ttl acme=2592000`) ages one tenant's spans out on its own
  compliance clock, deleting — not hiding — from segments, log, and payload
  files alike. This demo covers the "this specific data must go, and prove
  it" half; the TTL half needs no subject and no receipt.
- The receipt's `incomplete` verdict while the pin is held, and the `0/3/2`
  exit codes offline, come from the store's own verification walk; the demo
  prints them verbatim and asserts on them, it does not compute them.

## Runtime

Roughly 2 s at either scale — startup dominates, so `TRAZA_VANISH_SCALE=1`
barely moves it. The tokens are generated per run and die with the process;
nothing persists.
