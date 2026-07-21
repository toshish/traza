# Contributing to Traza

Thanks for helping build Traza. This document covers everything needed to go from a fresh clone to a mergeable pull request.

## Development setup

You need stable Rust (1.70 or newer, installed via [rustup](https://rustup.rs)) — and nothing else. There is no database to run, no container to start, no service to configure:

```sh
git clone https://github.com/toshish/traza.git
cd traza
cargo build
```

## The CI gate

`./ci.sh` is the whole CI story and the merge bar. It runs four gates in order and fails fast:

```sh
cargo fmt -- --check                                  # formatting
cargo clippy --all-targets --all-features -- -D warnings   # lints, warnings are errors
cargo build --release                                 # release build
cargo test                                            # test suite
```

Run it before pushing. A pull request is expected to arrive with `./ci.sh` green; reviewers will run it too.

## Test layout

- `tests/storage.rs` — integration tests for the storage engine (flush, crash recovery, filter correctness, TTL compaction).
- Unit tests live inline in the module they cover, in the usual `#[cfg(test)]` style.
- `src/bin/bench.rs` — the end-to-end benchmark. It builds and starts the release server, drives it over HTTP, and rewrites `BENCHMARKS.md` from its own measurements. It is not part of `ci.sh`; run it when a change could plausibly move performance.

Never edit `BENCHMARKS.md` by hand. If your change affects performance, regenerate it with `cargo run --release --bin bench` and include the run in your PR description.

## Pull request expectations

- **`./ci.sh` is green.** All four gates, no exceptions.
- **No new dependencies without justification.** Traza deliberately has two direct dependencies (`serde`, `serde_json`); everything else uses the standard library. A PR that adds a dependency must explain why the standard library cannot reasonably do the job, and what the dependency's own footprint is.
- **Public items are documented.** The crate denies `missing_docs`; clippy will hold you to it.
- **Focused diffs.** One logical change per PR; imperative mood in commit subjects ("Add X", "Fix Y").
- **Honest claims.** Performance and durability statements in docs must be backed by a benchmark run or a test.

## License

Traza is dual-licensed under MIT and Apache-2.0. Unless you explicitly state otherwise, any contribution you intentionally submit for inclusion is dual-licensed the same way, without additional terms.
