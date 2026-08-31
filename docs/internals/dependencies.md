# Dependencies

Traza's standing budget is stated in
[CONTRIBUTING](../../CONTRIBUTING.md): no new dependency without a written
justification covering **why the standard library cannot reasonably do the
job** and **what the dependency's own footprint is**. This file is where
those justifications live, one section per decision, so that the absence of
a dependency reads as a decision rather than an accident.

The count today: **two direct dependencies**, `serde` and `serde_json`. The
native wire is JSON in both directions and so are the annotation, eval,
tombstone and manifest files; hand-writing serialization for every API type
would be more code, and more wrong code, than the dependency. Everything
else — HTTP, the OTLP protobuf decoder, the WAL, SHA-256, the digests, the
Bloom filters — is the standard library, on purpose. The full supply chain
is the lockfile: 12 packages, Traza itself included.

---

## lz4_flex — segment and blob compression (accepted, ships in v0.24.0)

**The job.** [Format v7](../segment-format.md#format-v7-specification--ships-in-v0240)
compresses the segment records region in 128 KiB record-aligned blocks and
compresses payload blobs whole. The measured motivation is in the spec and in
[storage-comparison](../storage-comparison.md): the records region is where
segment bytes go, and nothing in Traza compresses anything today.

**Why the standard library cannot reasonably do this job.** It contains no
compression — no DEFLATE, no LZ77 machinery, nothing to build on; this is
not a case of "std can do it awkwardly". The std-only alternative is writing
a compressor and decompressor by hand. That was done for SHA-256, and the
comparison is instructive rather than encouraging: SHA-256 is about 160
lines of `src/payload.rs` against published FIPS test vectors, with no
performance requirement beyond "off the ingest path". An LZ4 codec is a
decoder over
length-and-offset-encoded input where a bounds bug is a buffer overrun, in
the hottest read path the store has, competing against a decade of tuning.
Hand-rolling it is more unsafe surface, more maintenance, and more risk than
the dependency it would avoid — which is exactly the test the budget asks.

**Footprint.**

- Pure Rust. The intended manifest line, exactly:

  ```toml
  lz4_flex = { version = "=0.14.0", default-features = false, features = ["std", "safe-encode", "safe-decode"] }
  ```

  `default-features = false` exists to drop the frame format (v7 does not
  use it), but it drops `std` and the safe implementations with it — which
  is why all three go back in by name. None of the three is optional in
  practice: without `safe-encode`/`safe-decode` the crate substitutes its
  unsafe fast paths, and choosing the safe ones is what keeps the crate
  compatible in spirit with this project's `#![forbid(unsafe_code)]` — the
  forbid applies to Traza's own crate, but choosing the safe implementation
  means not importing an unsafe one either.
- In that configuration it pulls **no transitive dependencies** — verified
  against `lz4_flex` 0.14.0, surveyed 2026-08, where every dependency is
  optional and none is activated by these features: direct dependencies go
  2 → 3, and the lockfile grows by exactly the one package, 12 → 13. (The
  default feature set would add the frame format and a hash dependency; v7
  needs neither.)
- **Version pinned exact (`=` in `Cargo.toml`).** Compressor output bytes
  are format bytes under the acceptance tests — see
  [the determinism rule](../segment-format.md#determinism-compressor-output-is-format-bytes)
  — so a routine `cargo update` must not be able to change what the encoder
  writes. Upgrading the pin is a deliberate re-baseline commit. The pin
  constrains the encoder only; any correct LZ4 decoder reads any valid
  stream.
- The dependency count is written down in more places than the two obvious
  ones — all correct today, all stale the day the dependency lands. The
  implementing PR updates every one:
  - `CONTRIBUTING.md` — "two direct dependencies" in the PR expectations;
  - `README.md` — "Two direct dependencies, twelve packages in the whole
    lockfile";
  - `docs/README.md` — "two dependencies" in the opening line;
  - `docs/storage-comparison.md` — three places: "Two dependencies, no
    `zstd`", "It would be a third dependency", and "two direct
    dependencies" in the footprint bullet;
  - `docs/operations/durability.md` — "carries two dependencies", inside
    the macOS-fsync argument;
  - `src/wal.rs` — the same argument, same phrase, in the module doc
    comment.

  A list like this rots, so the instruction is the grep, not the list: the
  implementing PR runs
  `grep -rn 'two dependencies\|two direct\|twelve packages\|third dependency'`
  and settles every hit. This document precedes the code by design.

**Rejected.**

- **`zstd` (the C binding).** Better ratio — the
  [projection](../storage-comparison.md#what-compression-would-buy) that
  motivated compression was computed with zstd, and on those inputs
  `gzip -6` measured 7–10% worse than `zstd -3`, with LZ4 expected below
  both on ratio. But the crate chain (`zstd` → `zstd-safe` → `zstd-sys`)
  plus its build machinery (`cc`, `jobserver`, and on some platforms
  `pkg-config`) adds roughly six lockfile packages at time of writing,
  puts a C compiler into every build of a project that currently builds with
  `cargo build` and nothing else, and moves the decode path for untrusted
  disk bytes behind FFI — precisely the unsafe surface the safe-decode
  choice above exists to avoid. Ratio is the one axis it wins, and v7's
  gates are stated in amplification and latency, not in matching zstd.
- **`ruzstd` (pure-Rust zstd).** Removes the C toolchain objection but not
  the fit objection: it is a decoder-first project whose encoder story is
  not what v7's write path should stand on, and its real destiny here is
  different — see the cold-tier ruling below. Adopting it now would spend a
  dependency on the wrong tier.
- **DEFLATE (`miniz_oxide`/`flate2`) and snappy.** DEFLATE decodes slower
  than LZ4 and measured 7–10% worse than `zstd -3` on ratio on the
  projection inputs — dominated on both axes for this workload. Snappy sits
  in LZ4's speed class without beating it, and its Rust implementations
  bring no advantage that would justify choosing the less common format.
  Neither offers a ratio-speed point that LZ4 and zstd between them do not
  already cover.

**Ruled while deciding this, so the absences are decisions:**

- **The zstd-with-dictionary cold tier is evidence-gated.** The gate is a
  real deployment where disk still binds *after* v7 ships and is measured.
  Until that exists, no second codec, no dictionary training, no `ruzstd`.
  v7's codec-id parameterization is what makes saying yes later cheap:
  a new codec id and configuration, not a format bump.
- **Outbound TLS is not being added.** No `rustls`, no native-TLS binding,
  nothing. TLS belongs to the future native-S3 replication phase and gets
  its own entry here when that phase is real; compressing segments requires
  exactly zero network code.
- **The HA track neither gates nor is gated by this.** Ship/follow/promote
  work proceeds independently of the v7 format, and nothing in this
  dependency decision waits on it.
