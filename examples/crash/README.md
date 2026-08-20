# Crash demo

`kill -9` mid-ingest, then the receipts. A client streams spans at a server
running `--durability wal`, the server is SIGKILLed while batches are still in
flight, and after a restart on the same directory every acknowledged span is
counted back out. Then the recovered store is backed up hot while it serves,
one byte of a segment file is flipped on disk, `GET /v1/verify` names the
damaged file, and the backup is restored. The
[durability contract](../../docs/operations/durability.md) and the
[backup mechanism](../../docs/operations/backup.md) are the claims under test;
the script asserts every one of them and exits non-zero if any fails.

```sh
examples/crash/run.sh
```

It builds `traza-server` if it is missing, works entirely in a throwaway
`mktemp` directory, and removes everything on exit. `python3` is the ingest
client and the JSON reader, `curl` carries the rest of the requests, and
nothing else is needed.

## The beats

1. **A promise, in writing.** The server starts with `--durability wal` and
   announces the contract on stderr; the script prints that line verbatim.
   Every ingest response repeats it: `{"accepted":N,"durability":"wal"}`.
2. **The kill.** `ingest.py` streams batches over one connection and counts a
   span only when a 200 acknowledges it. Once it reports roughly half the
   target — half plus a random offset, drawn fresh each run — `run.sh`
   delivers `kill -9`. No unwinding, no destructors, no flush on the way out.
   The client rides out the connection failure and records its final
   acknowledged count and the last acked span id.
3. **The restart.** Same data directory. The startup line shows the mode
   again, and `/v1/stats` shows the split: spans already sealed into segments
   versus spans replayed out of the write-ahead log into the buffer.
4. **The receipts.** `GET /v1/export?service=crash-test` streams every span
   back; the rows are counted client-side and cross-checked against the
   `X-Traza-Export-Count` trailer, and `X-Traza-Export-Complete: true` is
   required. The count must be at least the acknowledged count, and the last
   acked span must be present in its trace. The batch that was in flight
   without an acknowledgement gets its own line: how many of its spans
   actually survived, whatever that number turns out to be.
5. **Hot backup.** `POST /v1/backups/crash-demo` checkpoints, hard-links the
   pin, and verifies every digest — `"verified":true` is printed from the live
   response. The pin is copied out with `cp -a` and released.
6. **One flipped byte.** The middle byte of the largest `segment-*.seg` is
   XORed in place while the server is still running. `GET /v1/verify` answers
   `intact:false` and names the exact file in `problems`.
7. **Restore.** The damaged server is killed and a new one starts with
   `--restore` pointing at the backup copy, installing it into a fresh
   directory. `verify` comes back `intact:true` and the span count equals the
   count at pin time, exactly.

## Knobs

| Variable | Default | Meaning |
|---|---|---|
| `TRAZA_DEMO_PORT` | `8125` | Port for every server this demo starts |
| `TRAZA_CRASH_SPANS` | `60000` | Spans the client aims for; the kill lands near half. CI uses `8000` |
| `TRAZA_CRASH_BATCH` | `200` | Spans per ingest batch |

## Honest caveats

- **The macOS power-cut gap is real.** `kill -9` and a process panic cannot
  lose an acknowledged span because the data already reached the kernel; an
  OS crash cannot because the fsync had already pushed it to the drive, whose
  cache a kernel crash does not disturb. A macOS machine losing *power* can
  still lose an acknowledged write, because `fsync` there does not flush the
  drive's own cache and Traza does not reach for `F_FULLFSYNC`. On Linux there
  is no such gap. Wording per
  [durability.md](../../docs/operations/durability.md).
- **The in-flight batch has no promise.** A batch the client sent but no 200
  covered may or may not survive; the demo prints what happened, per run, and
  asserts nothing about it beyond arithmetic consistency.
- **SIGKILL, not a power cut.** This demo proves the process-death rows of the
  contract table. It cannot prove the power-loss rows on the machine it runs
  on, and does not claim to.
- The restore lands in a fresh directory to show the backup copy is
  self-sufficient; restoring over the original works the same way.
- A backup pin that survives holds real bytes, not references — what that
  means for deletion is [examples/vanish](../vanish)'s beat 7.

## Runtime

Measured on an Apple-silicon laptop: about 5 s at the default 60,000 spans,
about 2 s at the CI setting (`TRAZA_CRASH_SPANS=8000`).
