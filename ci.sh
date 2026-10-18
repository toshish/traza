#!/usr/bin/env bash
set -euo pipefail

# The merge bar. It covers BOTH halves of the tree: the Rust crate and the
# dashboard in ui/. Rust tooling cannot police the UI — a broken Vite build,
# or a source file with a stray control byte, sails straight past cargo.

# ---------------------------------------------------------------- source hygiene
# A literal NUL makes git treat a source file as binary: its diff disappears
# from review, and grep/blame stop working on it. This actually happened, so
# it is a gate, not a guideline.
# `tr | cmp` rather than `grep -P`, which is not portable (BSD grep).
nul_offenders=""
while IFS= read -r file; do
  [ -f "$file" ] || continue
  if ! LC_ALL=C tr -d '\000' < "$file" | cmp -s - "$file"; then
    nul_offenders="${nul_offenders}  ${file}"$'\n'
  fi
done <<EOF
$(git ls-files -- '*.rs' '*.js' '*.jsx' '*.ts' '*.tsx' '*.css' '*.html' \
                  '*.md' '*.sh' '*.py' '*.toml' '*.json' '*.yml' '*.yaml')
EOF
if [ -n "$nul_offenders" ]; then
  echo "ci: these tracked files contain a literal NUL byte, so git treats them" >&2
  echo "ci: as binary and hides them from diff, blame and grep:" >&2
  printf '%s' "$nul_offenders" >&2
  exit 1
fi

# ---------------------------------------------------------------- rust
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --release
cargo test --locked
cargo test --features object-storage --locked

# Classify release tags before they can select registry aliases.
python3 scripts/test-release-channel.py
python3 scripts/test-rust-notices.py
python3 scripts/test-smoke-object-cli.py
python3 scripts/generate-rust-notices.py --check
python3 scripts/test-object-s3.py
# The demos' agents refuse redirects, and the section below runs every demo
# against the binary just built, from a path with a space in it.
python3 scripts/test-demos.py

# ---------------------------------------------------------------- examples
# The demos are documentation people run, so each is a gate rather than a
# sample: they drive the real binary over the real endpoints and assert their
# own claims, and a change that breaks a surface they show should not merge
# green. Each runs at its documented smoke setting; guard, replay, prove,
# foresight and coding-agents have no scale knob and run as they are, in
# seconds each.
#
# They run through a symlink to this checkout whose path has a space in it:
# a checkout can live under such a path, and a script that splits its own
# path on spaces works from any path without one, so CI would never see it
# break otherwise. guard, replay, prove, foresight and coding-agents run the
# binary TRAZA_BIN names, so it is set here, never inherited: an older install
# exported in the caller's shell must not stand in for the one just built.
# foresight writes its corpus with the seed TRAZA_SEED_BIN names, so that is
# set too, to the seed the release build above also built: an exported older
# seed would write a corpus the change under test never touched.
if command -v python3 >/dev/null 2>&1; then
  demo_dir=$(mktemp -d "${TMPDIR:-/tmp}/traza-ci.XXXXXX")
  demos="$demo_dir/traza checkout"
  trap 'rm -f "$demos"; rmdir "$demo_dir"' EXIT
  ln -s "$PWD" "$demos"
  built="$demos/target/release/traza-server"
  TRAZA_DEMO_PORT=${TRAZA_DEMO_PORT:-8123} "$demos/examples/mcp-demo/run.sh" >/dev/null
  echo "ci: mcp demo ran"
  TRAZA_SWARM_SECONDS=8 "$demos/examples/swarm/run.sh" >/dev/null
  echo "ci: swarm demo ran"
  TRAZA_CRASH_SPANS=8000 "$demos/examples/crash/run.sh" >/dev/null
  echo "ci: crash demo ran"
  TRAZA_NEEDLE_SPANS=60000 "$demos/examples/needle/run.sh" >/dev/null
  echo "ci: needle demo ran"
  TRAZA_INCIDENT_SCALE=2 "$demos/examples/incident/run.sh" >/dev/null
  echo "ci: incident demo ran"
  TRAZA_VANISH_SCALE=1 "$demos/examples/vanish/run.sh" >/dev/null
  echo "ci: vanish demo ran"
  TRAZA_BIN="$built" "$demos/examples/guard/run.sh" >/dev/null
  echo "ci: guard demo ran"
  TRAZA_BIN="$built" "$demos/examples/replay/run.sh" >/dev/null
  echo "ci: replay demo ran"
  TRAZA_BIN="$built" "$demos/examples/prove/run.sh" >/dev/null
  echo "ci: prove demo ran"
  TRAZA_SEED_BIN="$demos/target/release/seed" TRAZA_BIN="$built" \
    "$demos/examples/foresight/run.sh" >/dev/null
  echo "ci: foresight demo ran"
  TRAZA_BIN="$built" "$demos/examples/coding-agents/run.sh" >/dev/null
  echo "ci: coding-agents demo ran"
else
  echo "ci: skipping the demos (python3 not found)"
fi

# ---------------------------------------------------------------- client libraries
# sdk/python and sdk/typescript are typed clients for the agent-native routes,
# and each suite spawns the release binary built above, so a change to a wire
# shape a client reads fails here rather than in someone's agent. Pointed at
# target/release on purpose, never at an exported TRAZA_SERVER_BIN: the gate
# tests what it just built. Both suites together take seconds. The Python
# client is standard library only, and python3 is already required above, so
# its suite always runs; the TypeScript suite needs Node 20+ and npm, to
# install its one dev dependency (tsc), and is skipped without them.
# The metadata check comes first: a version bump that missed a client, or a
# stale LICENSE or NOTICE copy, is named there rather than discovered later.
python3 scripts/check-sdk-metadata.py
sdk_server="$PWD/target/release/traza-server"
TRAZA_SERVER_BIN="$sdk_server" python3 -m unittest discover -s sdk/python/tests
echo "ci: python client tests ran"
node_major=""
if command -v node >/dev/null 2>&1; then
  node_major=$(node --version | sed 's/^v//; s/\..*//')
fi
if [ -n "$node_major" ] && [ "$node_major" -ge 20 ] && command -v npm >/dev/null 2>&1; then
  (
    cd sdk/typescript
    npm ci --no-audit --no-fund --loglevel=error
    TRAZA_SERVER_BIN="$sdk_server" npm test --silent
  )
  echo "ci: typescript client tests ran"
else
  echo "ci: skipping the typescript client tests (Node 20+ with npm not found)"
fi

# ---------------------------------------------------------------- dashboard
# Node is required: the dashboard is a first-class part of the product, and a
# UI that does not build must not merge green. Set TRAZA_SKIP_UI=1 only for a
# deliberate Rust-only run.
if [ "${TRAZA_SKIP_UI:-0}" = "1" ]; then
  echo "ci: skipping the dashboard build (TRAZA_SKIP_UI=1)"
  exit 0
fi

if ! command -v npm >/dev/null 2>&1; then
  echo "ci: npm not found. Install Node (see ui/.nvmrc for the supported version)," >&2
  echo "ci: or run with TRAZA_SKIP_UI=1 to check only the Rust crate." >&2
  exit 1
fi

required_node=$(tr -d 'v \n' < ui/.nvmrc)
actual_node=$(node --version | tr -d 'v')
if [ "${actual_node%%.*}" -lt "${required_node%%.*}" ]; then
  echo "ci: Node ${actual_node} is older than the supported ${required_node} (ui/.nvmrc)" >&2
  exit 1
fi

(
  cd ui
  npm ci
  # Shipped-dashboard dependencies must be clean; development-tool advisories
  # are printed for visibility but do not block the gate.
  npm audit --omit=dev --audit-level=high
  npm audit --audit-level=high || echo "ci: development-dependency advisories above are non-blocking"
  npm test
  npm run build
)
