#!/bin/sh
# A million spans through the front door, then find one sentence.
#
# Ingests ~1,000,000 lean spans over real HTTP into a throwaway store on this
# machine, then answers needle queries in milliseconds. Every latency below is
# measured live, every count is asserted, and the script exits non-zero if a
# claim fails to hold.
#
#   examples/needle/run.sh
#
set -eu

# multiprocessing re-imports its main module, which would litter __pycache__.
export PYTHONDONTWRITEBYTECODE=1

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
here=$root/examples/needle
cd "$root"

server=./target/release/traza-server
data=$(mktemp -d "${TMPDIR:-/tmp}/traza-needle.XXXXXX")
port=${TRAZA_DEMO_PORT:-8126}
spans=${TRAZA_NEEDLE_SPANS:-1000000}
pid=""

cleanup() {
  [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  rm -rf "$data"
}
trap cleanup EXIT
# INT/TERM get their own trap: bash 3.2 restarts a `read` interrupted by a
# trapped signal, so Ctrl-C during TRAZA_NEEDLE_HOLD=1 would clean up and
# then keep waiting. Exiting explicitly here ends the run, non-zero.
trap 'cleanup; trap - EXIT; exit 130' INT
trap 'cleanup; trap - EXIT; exit 143' TERM

command -v python3 >/dev/null 2>&1 || {
  echo "this demo uses python3 (stdlib only) to flood the server and time the probes" >&2
  exit 1
}
command -v curl >/dev/null 2>&1 || {
  echo "this demo drives the server with curl" >&2
  exit 1
}

bold() { printf '\n\033[1m%s\033[0m\n' "$*"; }
dim() { printf '\033[2m%s\033[0m\n' "$*"; }

probe() { python3 "$here/probe.py" --port "$port" "$@"; }

# ------------------------------------------------------------------ setup

if [ ! -x "$server" ]; then
  echo "building traza-server…"
  cargo build --release --bin traza-server
fi

cat <<BANNER

┌──────────────────────────────────────────────────────────────────────┐
│  Traza needle demo — a haystack in, one sentence out                 │
└──────────────────────────────────────────────────────────────────────┘

  server:  http://127.0.0.1:$port   (throwaway store in $data)
  client:  python3 stdlib over plain HTTP — the same front door you get
  target:  $spans spans in batches of 1,000, one needle mid-flood
BANNER

bold "▸ start"
dim "  --durability wal --profile throughput: the documented bulk-backfill profile."
dim "  It sets flush-spans (30,000) and the wal commit window (500 µs) — never durability."
"$server" --data-dir "$data" --port "$port" --durability wal --profile throughput >"$data/server.log" 2>&1 &
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
# The probe answering proves a server is up — not that it is ours: a foreign
# listener answers before our child has even tried the port. Wait for the
# child to either claim the port in its log or lose the bind and die, then
# assert it is alive — a dead child means something else owns the port, and
# the flood below would land a million spans in a stranger's store.
for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
  kill -0 "$pid" 2>/dev/null || break
  grep -q "listening on" "$data/server.log" 2>/dev/null && break
  sleep 0.25
done
kill -0 "$pid" 2>/dev/null || {
  echo "another server is already on port $port (set TRAZA_DEMO_PORT)" >&2
  exit 1
}
# Whether the dashboard deep link at the end will actually render: GET /
# answers 200 only once ui/dist has been built.
ui_ok=""
[ "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$port/")" = 200 ] && ui_ok=yes
dim "  up."

bold "▸ flood — $spans spans over real HTTP"
dim "  POST /v1/spans from 8 processes in batches of 1,000: six services, timestamps"
dim "  spread across the last 30 days, one needle inserted mid-flood. The rate below"
dim "  measures this python client as much as the server — see the README."
if [ "${TRAZA_NEEDLE_HOLD:-0}" = 1 ] && [ -n "$ui_ok" ]; then
  dim "  TRAZA_NEEDLE_HOLD=1 — open http://127.0.0.1:$port/#/overview now to watch"
  dim "  the ingest sparkline take the flood; starting in 6 seconds."
  sleep 6
fi
python3 "$here/flood.py" --port "$port" --spans "$spans"

bold "▸ flush"
dim "  POST /v1/flush seals what is still buffered, so every probe below reads segments."
curl -sS -X POST "http://127.0.0.1:$port/v1/flush" >/dev/null

bold "▸ trace lookup"
dim "  GET /v1/traces/needle-trace-1, 200 round-trips on one keep-alive connection."
probe lookup

bold "▸ attribute filter"
dim "  GET /v1/spans?attr.needle=true — one exact attribute match against the whole store."
probe attr

bold "▸ content search"
dim "  GET /v1/spans?q=aubergine%20midnight — word search over every stored string value."
probe content

bold "▸ a word the store has never seen"
dim "  GET /v1/spans?q=xylotheque — the per-segment content index prunes every segment."
probe absent

bold "▸ time-window pruning"
dim "  GET /v1/spans?service=checkout&since=<2 days ago> — a segment whose stored time"
dim "  range cannot match is skipped whole, and the cost object counts the skips."
probe window

bold "▸ whole-corpus aggregate"
dim "  GET /v1/stats/duration with no filter — every span's duration folded in one pass."
probe aggregate --expect-spans "$spans"

bold "▸ the store it did that against"
dim "  from GET /v1/stats — segments are uncompressed JSON plus indexes, the honest"
dim "  trade behind the latencies above (docs/storage-comparison.md)."
probe store --expect-spans "$spans"

bold "▸ the same needle, in the dashboard"
printf '  http://127.0.0.1:%s/#/traces?c=aubergine%%20midnight\n' "$port"
dim "  the query-cost line under the results is read off the same response envelope."
if [ -z "$ui_ok" ]; then
  dim "  (the dashboard is not built, so that link answers a JSON 404 for now."
  dim "   cd ui && npm ci && npm run build — the server picks it up without a restart.)"
fi
if [ "${TRAZA_NEEDLE_HOLD:-0}" = 1 ]; then
  dim "  TRAZA_NEEDLE_HOLD=1 — the server stays up for the link above; press Enter to clean up."
  read -r _hold
else
  dim "  (the store is removed on exit; TRAZA_NEEDLE_HOLD=1 pauses here so the link works.)"
fi

cat <<'CLOSING'

┌──────────────────────────────────────────────────────────────────────┐
│  Done — the store and its directory are removed on exit.             │
│                                                                      │
│  Every count above was asserted, not narrated: one span in the       │
│  needle trace, one hit for the attribute, one for the sentence,      │
│  zero for the word that was never ingested, and the aggregate and    │
│  the store's record count both exactly the number of spans the       │
│  server acknowledged.                                                │
│                                                                      │
│  Reference: docs/guide/http-api.md                                   │
└──────────────────────────────────────────────────────────────────────┘
CLOSING
