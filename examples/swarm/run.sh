#!/bin/sh
# A live agent cockpit in under a minute.
#
# Starts a Traza server on a throwaway store, then streams a simulated
# agent platform into it in real time — three services, wall-clock
# timestamps — so the dashboard's live tail, waterfalls, sessions and
# cost analytics are all breathing while you watch.
#
#   examples/swarm/run.sh
#
# Runs until Ctrl-C by default (TRAZA_SWARM_SECONDS bounds it), verifies
# its own claims against the API at the 6-second mark, and cleans up on
# exit.

set -eu

# multiprocessing re-imports its main module, which would litter __pycache__.
export PYTHONDONTWRITEBYTECODE=1

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
here=$root/examples/swarm
cd "$root"

server=./target/release/traza-server
data=$(mktemp -d "${TMPDIR:-/tmp}/traza-swarm.XXXXXX")
port=${TRAZA_DEMO_PORT:-8124}
pid=""

cleanup() {
  [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  rm -rf "$data"
}
trap cleanup EXIT INT TERM

command -v python3 >/dev/null 2>&1 || {
  echo "this demo uses python3 (stdlib only) to generate the span stream" >&2
  exit 1
}
command -v curl >/dev/null 2>&1 || {
  echo "this demo drives the server with curl" >&2
  exit 1
}

if [ ! -x "$server" ]; then
  echo "building traza-server…"
  cargo build --release --bin traza-server
fi

"$server" --data-dir "$data" --port "$port" --durability wal >"$data/server.log" 2>&1 &
pid=$!
ready=""
for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
  if curl -sS -o /dev/null "http://127.0.0.1:$port/v1/stats" 2>/dev/null; then
    ready=yes
    break
  fi
  sleep 0.25
done
[ -n "$ready" ] || {
  echo "the server did not come up on port $port (set TRAZA_DEMO_PORT)" >&2
  exit 1
}
# The poll above answers as soon as *something* speaks HTTP on the port.
# Make sure that something is our child: it announces its successful bind
# in the log, and a child that lost the port race dies instead. (The log
# check matters — a dead child lingers as a zombie until the shell reaps
# it, and kill -0 alone still passes for a zombie.)
announced=""
for _ in 1 2 3 4 5 6 7 8; do
  if grep -q "listening on" "$data/server.log" 2>/dev/null; then
    announced=yes
    break
  fi
  sleep 0.25
done
if [ -z "$announced" ] || ! kill -0 "$pid" 2>/dev/null; then
  echo "another server is already on port $port (set TRAZA_DEMO_PORT)" >&2
  exit 1
fi

# The API is up either way, but the dashboard the deep links point at is a
# built artifact (ui/dist). Probe it so the banner can be honest on a fresh
# clone, and so TRAZA_SWARM_OPEN never opens a JSON 404.
dash=""
if [ "$(curl -sS -o /dev/null -w '%{http_code}' "http://127.0.0.1:$port/" 2>/dev/null)" = "200" ]; then
  dash=yes
fi

cat <<BANNER

┌──────────────────────────────────────────────────────────────────────┐
│  Traza swarm demo — a live agent cockpit in under a minute           │
└──────────────────────────────────────────────────────────────────────┘

  Three simulated agent services stream spans with wall-clock
  timestamps into a throwaway store. Open these while it runs:

    live tail:  http://127.0.0.1:$port/#/tail
    overview:   http://127.0.0.1:$port/#/overview
    analytics:  http://127.0.0.1:$port/#/analytics

  A trace deep link prints below each time a research fan-out lands,
  and a conversation link prints at the verification gate.
BANNER

if [ -z "$dash" ]; then
  printf '\033[2m%s\n%s\033[0m\n' \
    "  (the dashboard is not built, so these links answer a JSON 404 until" \
    "   cd ui && npm ci && npm run build — picked up without a restart.)"
fi

if [ "${TRAZA_SWARM_OPEN:-0}" = "1" ] && [ -n "$dash" ]; then
  if command -v open >/dev/null 2>&1; then
    open "http://127.0.0.1:$port/#/overview"
  elif command -v xdg-open >/dev/null 2>&1; then
    xdg-open "http://127.0.0.1:$port/#/overview"
  fi
fi

python3 "$here/swarm.py" "$port"

cat <<'CLOSING'

┌──────────────────────────────────────────────────────────────────────┐
│  Done — the server and its throwaway store are removed on exit.      │
│                                                                      │
│  Every server-read number above was live: acks summed from the       │
│  server's responses, sessions, costs and trace shapes read back      │
│  over the API. The workload was simulated; the measurements of it    │
│  were not.                                                           │
│                                                                      │
│  Reference: docs/guide/http-api.md · docs/guide/trace-browser.md     │
└──────────────────────────────────────────────────────────────────────┘
CLOSING
