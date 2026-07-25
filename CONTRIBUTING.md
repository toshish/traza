# Contributing to Traza

Thanks for helping build Traza. This document covers everything needed to go from a fresh clone to a mergeable pull request.

## Development setup

You need stable Rust (1.70 or newer, installed via [rustup](https://rustup.rs)) to build the server, and Node 22 or newer (see [`ui/.nvmrc`](ui/.nvmrc)) to build the dashboard. There is no database to run, no container to start, no service to configure:

```sh
git clone https://github.com/toshish/traza.git
cd traza
cargo build
```

The dashboard is a separate build artifact, never compiled into the binary — the server runs fine without it and serves the API only. See [`ui/README.md`](ui/README.md).

## The CI gate

`./ci.sh` is the whole CI story and the merge bar — there is no GitHub Actions workflow. It runs in order and fails fast:

```sh
# source hygiene: no tracked source file may contain a literal NUL byte
cargo fmt -- --check                                       # formatting
cargo clippy --all-targets --all-features -- -D warnings   # lints, warnings are errors
cargo build --release                                      # release build
cargo test                                                 # test suite
(cd ui && npm ci && npm test && npm run build)             # dashboard
```

The NUL-byte check is a gate rather than a guideline because it actually happened: a literal NUL makes git treat a source file as binary, its diff disappears from review, and grep and blame stop working on it.

The dashboard is a first-class part of the product, so a UI that does not build — or whose vitest suite fails — must not merge green. `TRAZA_SKIP_UI=1` skips that step for a deliberate Rust-only run, not for a merge.

Run `./ci.sh` before pushing. A pull request is expected to arrive with it green; reviewers will run it too.

## Tests

Unit tests live inline in the module they cover, in the usual `#[cfg(test)]` style. Integration tests live in `tests/`, one file per concern; the ones that exercise the HTTP contract spawn the real `traza-server` binary and drive it over a socket.

**[docs/internals/testing.md](docs/internals/testing.md) is required reading before you add one.** The short version:

- **A test must be shown to FAIL when the behaviour it guards is broken.** Mutate the code, watch it go red, revert, watch it go green. A test that has never failed is a claim, not evidence. Several files in `tests/` exist in their current form specifically because the previous version passed against broken code.
- Beware self-consistent fixtures — if the expected and actual values are computed the same way, you are testing arithmetic. Feed fixtures through the real reader.
- Concurrency bugs need concurrent tests. Several of the engine's [invariants](docs/internals/invariants.md) have failure modes a sequential test cannot reach.

If you are changing the engine, read [docs/internals/invariants.md](docs/internals/invariants.md) first. Those rules are load-bearing and easy to break unknowingly.

## Benchmarks

`cargo run --release --bin bench` measures the canonical corpus and rewrites `BENCHMARKS.md`. `cargo run --release --bin ingest-bench` measures the ingest matrix and rewrites `INGEST-BENCHMARK.md`. Neither is part of `ci.sh`; run the relevant one when a change could plausibly move performance, and include the run in your PR description.

**Never edit `BENCHMARKS.md` or `INGEST-BENCHMARK.md` by hand.** They are generated. See [docs/internals/benchmarking.md](docs/internals/benchmarking.md) for the flags and the rules on reporting a measurement honestly.

## Documentation

Documentation lives in [`docs/`](docs/README.md), organised by audience: `guide/` for users, `operations/` for operators, `internals/` for developers. The README is an overview that routes to them, not a replacement for them.

- **Every code example must actually work.** Run it. A `curl` that 404s or a Rust snippet that no longer compiles is worse than no example.
- **Check routes and parameters against [`src/bin/traza-server.rs`](src/bin/traza-server.rs)**, which is the authoritative source, rather than against other documentation that may have drifted.
- **Never state a performance number, a guarantee, or a behaviour you have not verified** in the code or in a committed measurement file. If you are unsure, say so rather than writing a confident sentence.

## Pull request expectations

- **`./ci.sh` is green.** Every gate, no exceptions.
- **No new dependencies without justification.** Traza deliberately has two direct dependencies (`serde`, `serde_json`); everything else uses the standard library. A PR that adds a dependency must explain why the standard library cannot reasonably do the job, and what the dependency's own footprint is.
- **Public items are documented.** The crate denies `missing_docs`; clippy will hold you to it. `#![forbid(unsafe_code)]` is not negotiable.
- **Focused diffs.** One logical change per PR; imperative mood in commit subjects ("Add X", "Fix Y").
- **Honest claims.** Performance and durability statements in docs must be backed by a benchmark run or a test.

## License

Traza is licensed under Apache-2.0. Unless you explicitly state otherwise, any contribution you intentionally submit for inclusion is licensed the same way, without additional terms.
