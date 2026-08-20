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
                  '*.md' '*.sh' '*.toml' '*.json' '*.yml' '*.yaml')
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
cargo test

# ---------------------------------------------------------------- examples
# The demos are documentation people run, so each is a gate rather than a
# sample: they drive the real binary over the real endpoints and assert their
# own claims, and a change that breaks a surface they show should not merge
# green. Each runs at its documented smoke setting; the whole tour stays
# under a minute.
if command -v python3 >/dev/null 2>&1; then
  TRAZA_DEMO_PORT=${TRAZA_DEMO_PORT:-8123} ./examples/mcp-demo/run.sh >/dev/null
  echo "ci: mcp demo ran"
  TRAZA_SWARM_SECONDS=8 ./examples/swarm/run.sh >/dev/null
  echo "ci: swarm demo ran"
  TRAZA_CRASH_SPANS=8000 ./examples/crash/run.sh >/dev/null
  echo "ci: crash demo ran"
  TRAZA_NEEDLE_SPANS=60000 ./examples/needle/run.sh >/dev/null
  echo "ci: needle demo ran"
  TRAZA_INCIDENT_SCALE=2 ./examples/incident/run.sh >/dev/null
  echo "ci: incident demo ran"
  TRAZA_VANISH_SCALE=1 ./examples/vanish/run.sh >/dev/null
  echo "ci: vanish demo ran"
else
  echo "ci: skipping the demos (python3 not found)"
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
