# Testing

## The standard: a test must be shown to fail

**A test that has never failed is a claim, not evidence.**

Before a test is considered to guard a behaviour, break the behaviour and watch
the test go red. Revert, watch it go green. If it stays green against broken
code, it does not guard anything — no matter how thorough its assertions look.

This is not aspirational. Several files in `tests/` were written or rewritten
specifically because they passed against broken code:

- **`tests/segment_format_acceptance.rs`** built its own "deliberately
  independent" byte fixture. Because that fixture was only ever compared
  against itself and never fed to the real reader, it drifted into a layout
  Traza has never written — a `u32` header length at offset 12, three sections
  instead of four, a `.trz2` extension — and **passed continuously while
  asserting nothing about the engine**. Feeding it to `Segment::from_bytes`
  failed with `Corrupt("invalid v2 header length")`. The same blind spot hid a
  magic-bytes disagreement: the encoder wrote `TRAZAV2` while the test and the
  format doc expected `TRAZASEG`, and the two never met. The file now parses
  bytes from the real `segment::encode`, round-trips them through the real
  reader, and pins the magic to the real constant so they cannot drift again.
- The same rewrite removed a `reopen_persistence` case that wrote bytes and
  read them back asserting equality — a test of `fs`, not of Traza.
- **`reads_never_miss_committed_spans`** is documented in
  [`INGEST-BENCHMARK.md`](../../INGEST-BENCHMARK.md) as *not* catching the
  visibility regression an unlocked seal would introduce: its writer is
  single-threaded and `flush()` is synchronous end to end, so the window never
  opens. That is why moving sealing off the writer lock was reverted rather
  than merged — the suite as it stands would pass over the regression.

The lesson generalizes. When you add a test:

- **Mutate the code it guards**, not the test. Flip a comparison, drop a lock,
  skip an fsync, return early. Confirm red.
- **Beware self-consistent fixtures.** If your expected value and your actual
  value are computed the same way, you are testing arithmetic. Feed fixtures
  through the real reader.
- **Concurrency bugs need concurrent tests.** Invariants 2, 6, and 9 in
  [invariants](invariants.md) all have failure modes a sequential test cannot
  reach. A synchronous `flush()` never opens the window a real flush opens.
- **Prefer a real oracle.** SIGKILL for durability. An independent encoder for
  a wire format. Opening the data directory with a second `Store` for "the
  server owns no storage".

## Layout

Unit tests live inline in the module they cover, in the usual `#[cfg(test)]`
style — currently in `auth`, `media`, `metrics`, `payload`, `seed`, `semconv`,
`ui`, and `wal`.

Integration tests live in `tests/`, one file per concern. Files that exercise
the HTTP contract spawn the **real** `traza-server` binary and drive it over a
socket; files that exercise the engine use `traza::Store` directly.

| File | Tests | Covers |
|---|---:|---|
| [`storage.rs`](../../tests/storage.rs) | 33 | The engine end to end: persistence, crash recovery, filter correctness, ordering, TTL compaction (including that expiry reaches the write-ahead log, so an expired span cannot return across a restart), log-corruption refusal, the flush bounds that keep the log finite, pinned snapshot views, the retryability of a failed expiry under injected I/O faults, concurrency, second-open rejection |
| [`scenarios.rs`](../../tests/scenarios.rs) | 14 | Derived views over the deliberately messy seed corpus — three attribute dialects, agent trees, multimodal payloads, linked retries, fan-out. Guards rollups that are right on a tidy fixture and wrong on real data |
| [`keepalive.rs`](../../tests/keepalive.rs) | 11 | Persistent connections. Not "is it faster" but "does the server ever leave bytes on a socket it intends to reuse" — request smuggling is the risk. Asserts the `Connection` header **and** the behaviour, because a response that says `keep-alive` while the server closes desynchronizes any client that believes it |
| [`payloads_annotations.rs`](../../tests/payloads_annotations.rs) | 10 | Payload offloading, content addressing, annotations, and streaming export |
| [`ingest_hardening.rs`](../../tests/ingest_hardening.rs) | 10 | Adversarial input: lying `Content-Length`, oversized headers, silent peers, degenerate ids at the library boundary, query-parameter extremes |
| [`durability.rs`](../../tests/durability.rs) | 9 | The acknowledgement contract, **proven by SIGKILL** — no unwinding, no destructors, no flush on the way out. Each mode held to exactly what it claims, including that `buffered` is verified lossy rather than accidentally durable |
| [`analytics.rs`](../../tests/analytics.rs) | 8 | Sessions and LLM aggregation across the write buffer, sealed segments, window boundaries, and reopen |
| [`compaction.rs`](../../tests/compaction.rs) | 10 | Size-tiered compaction bounding segment count **without changing a single answer**. Every test checks the data as well as the count — losing the newest version of a re-ingested span would be far worse than the slow search it fixes. Two are concurrent: reads and ingest must not wait out a merge, and a flush landing mid-merge must still supersede merged content |
| [`server_on_engine.rs`](../../tests/server_on_engine.rs) | 7 | The real server over its real wire contract, plus opening its data directory with `traza::Store` and comparing — the oracle for "the server has no private store" |
| [`segment_format_acceptance.rs`](../../tests/segment_format_acceptance.rs) | 5 | The on-disk format, hand-parsed at fixed offsets from bytes the **real encoder** produced, round-tripped through the real reader. Each test emits a JSON evidence record |
| [`auth.rs`](../../tests/auth.rs) | 4 | The bearer-auth matrix at process level: loopback open by default; 401/403/200 across ingest, OTLP, and flush |
| [`dashboard.rs`](../../tests/dashboard.rs) | 4 | The SPA served from disk, asset content types, path-traversal refusal, shell loading without credentials while the API stays gated, and a missing build degrading to a helpful 404 |
| [`openllmetry_conformance.rs`](../../tests/openllmetry_conformance.rs) | 4 | Traceloop conventions through **both** ingest surfaces landing in sessions and rollups with no client-side renaming. The regression guard for the gap where an OpenLLMetry app stored fine but registered zero tokens, cost, calls, or sessions |
| [`otlp_protobuf.rs`](../../tests/otlp_protobuf.rs) | 3 | Binary protobuf against the real server, using the test's **own** protobuf encoder — an independent implementation, so agreement is evidence about the format rather than a shared bug |
| [`otlp_conformance.rs`](../../tests/otlp_conformance.rs) | 2 | OTLP/HTTP JSON driving the real binary end to end |
| [`llm_semantics.rs`](../../tests/llm_semantics.rs) | 2 | Every query recipe in [`docs/llm-semantics.md`](../llm-semantics.md), through both ingest paths. The documentation's executable oracle |

Note the last row: `docs/llm-semantics.md` is tested. If you change a recipe
there, change the test.

## Running the suite

```sh
cargo test                     # everything
cargo test --test storage      # one integration file
cargo test --release --test durability   # process-level tests are faster in release
```

Tests that spawn the server need `target/release/traza-server` (or the debug
equivalent) to exist; `./ci.sh` builds it before testing.

Process-level tests bind `--port 0` and read the actual port from the server's
stderr announcement, so they do not collide when run in parallel. Each uses its
own temporary data directory, because a data directory has exactly one writer.

## The gate

[`./ci.sh`](../../ci.sh) is the whole CI story — there is no GitHub Actions
workflow. It must be green before a change is finished. It runs, in order:

1. **Source hygiene** — no tracked source file may contain a literal NUL byte.
   A NUL makes git treat a file as binary: its diff disappears from review and
   grep and blame stop working on it. This actually happened, which is why it
   is a gate.
2. `cargo fmt -- --check`
3. `cargo clippy --all-targets --all-features -- -D warnings`
4. `cargo build --release`
5. `cargo test`
6. `cd ui && npm ci && npm test && npm run build`

The dashboard is a first-class part of the product: a UI that does not build,
or whose vitest suite fails, does not merge green. `TRAZA_SKIP_UI=1` skips
step 6 for a deliberate Rust-only run — not for a merge.

The benchmarks are **not** part of `ci.sh`; run them when a change could
plausibly move performance. See [benchmarking](benchmarking.md).
