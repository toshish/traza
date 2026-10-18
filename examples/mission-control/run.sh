#!/bin/sh
# Mission Control: steer live agent runs from the dashboard.
#
# Starts a Traza server on a throwaway store, gives it the history its
# foresight learns from, then runs simulated agents in real time — each asks
# the run guard before every step, hears nudges there, and holds on a stop
# until someone releases it. It checks every claim the dashboard's Live and
# Run screens are built on, prints the links, and keeps the agents running
# for you to steer until Ctrl-C.
#
#   examples/mission-control/run.sh
#
# It builds what it needs (or uses TRAZA_BIN), works in a throwaway
# directory, and cleans up on exit. It exits non-zero if any claim fails.

set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
here=$root/examples/mission-control

# TRAZA_BIN is the caller's, so it resolves before the cd below: a path from
# the caller's directory, a bare name from PATH.
server=./target/release/traza-server
if [ -n "${TRAZA_BIN:-}" ]; then
  case $TRAZA_BIN in
  /*) server=$TRAZA_BIN ;;
  */*) dir=$(CDPATH= cd -- "$(dirname -- "$TRAZA_BIN")" 2>/dev/null && pwd) &&
    server=$dir/$(basename -- "$TRAZA_BIN") || server= ;;
  *) server=$(command -v "$TRAZA_BIN" 2>/dev/null) || server= ;;
  esac
  case $server in /*) ;; *) server= ;; esac
  [ -n "$server" ] && [ -f "$server" ] && [ -x "$server" ] || {
    echo "TRAZA_BIN=$TRAZA_BIN is not an executable (a path, or a command on PATH)" >&2
    exit 1
  }
fi
cd "$root"

# A function, not a command kept in a string: expanding the string would
# split "$here" wherever the checkout's path has a space.
agents() { python3 "$here/agents.py" "$@"; }
tmp=$(mktemp -d "${TMPDIR:-/tmp}/traza-mission.XXXXXX")
port=${TRAZA_DEMO_PORT:-8134}
url=http://127.0.0.1:$port
seconds=${TRAZA_LIVE_SECONDS:-0}
tempo=${TRAZA_LIVE_TEMPO:-1}
pid=""

cleanup() {
  [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  rm -rf "$tmp"
}
trap cleanup EXIT
trap 'printf "\ninterrupted\n" >&2; exit 130' INT
trap 'exit 143' TERM

command -v python3 >/dev/null 2>&1 || {
  echo "this demo's agents are python3 (standard library only)" >&2
  exit 1
}
command -v curl >/dev/null 2>&1 || {
  echo "this demo waits for the server with curl" >&2
  exit 1
}

fail() { printf '\nFAILED: %s\n' "$*" >&2; exit 1; }

# Process state, not kill -0: a child this shell has not reaped is a zombie,
# and kill -0 succeeds on it. Gone or Z means exited.
alive() {
  if ! command -v ps >/dev/null 2>&1; then
    kill -0 "$1" 2>/dev/null
    return
  fi
  case $(ps -o stat= -p "$1" 2>/dev/null | tr -d ' ') in '' | Z*) return 1 ;; esac
}

start() {
  : >"$tmp/server.log"
  "$server" --data-dir "$tmp/data" --port "$port" >"$tmp/server.log" 2>&1 &
  pid=$!
  tries=0
  # "listening on" is printed only after the server's own bind succeeded, so
  # a stranger squatting on the port cannot pass for it.
  until grep -q 'listening on' "$tmp/server.log" 2>/dev/null &&
    curl -sS -o /dev/null "$url/healthz" 2>/dev/null; do
    alive "$pid" || {
      sed 's/^/  /' "$tmp/server.log" >&2
      fail "the server exited during startup — is port $port taken? (set TRAZA_DEMO_PORT)"
    }
    tries=$((tries + 1))
    [ "$tries" -lt 40 ] || fail "the server did not come up on port $port (set TRAZA_DEMO_PORT)"
    sleep 0.25
  done
}

stop() {
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
  pid=""
}

if [ ! -x "$server" ]; then
  echo "building traza-server…"
  cargo build --release --bin traza-server
fi
mkdir -p "$tmp/data"

# The history: completed runs, written and then the server restarted, so the
# live view opens empty — runs written now would be listed as live for the
# whole live window — while the Fate Map reads them from the store.
start
agents history "$url"
stop
start

dashboard="the dashboard"
if [ "$(curl -sS -o /dev/null -w '%{http_code}' "$url/" 2>/dev/null)" != 200 ]; then
  dashboard="the dashboard (build it once: cd ui && npm ci && npm run build — the server picks it up)"
fi

cat <<BANNER

┌──────────────────────────────────────────────────────────────────────┐
│  Traza Mission Control — steer live agent runs                       │
└──────────────────────────────────────────────────────────────────────┘

  server:   $url   (an open store, on loopback; throwaway)
  open:     $url/#/live      $dashboard

  Support agents ask the guard before every step. A run on a path that has
  failed before is held by its history; nudge it, approve it, or pull the
  kill switch from the Live screen, and watch the agent hear you.

BANNER

if [ "$seconds" = 0 ]; then
  echo "  checking every claim, then running until Ctrl-C"
else
  echo "  checking every claim, for ${seconds}s"
fi
agents live "$url" --verify --seconds "$seconds" --tempo "$tempo"
