#!/bin/sh
# A run's fate, read from every run before it — and a fuse that acts on it.
#
# The behaviour corpus is loaded: a support agent whose runs take one of four
# paths, each succeeding at a known rate. The Fate Map finds the path that
# kills runs — re-running an empty search — with its Wilson interval and the
# Fisher test behind it. A new run starts down that path; foresight calls it
# likely to fail two steps in, while it has not failed. A fuse policy with
# min_success_odds then stops it at the pre-call check, before the third
# search, with the reason; a run on the healthy path is allowed every step.
# Last, the map scores its own predictions.
#
#   examples/foresight/run.sh
#
# It builds what it needs (or uses TRAZA_BIN, and the seed binary beside it),
# works in a throwaway directory, and cleans up on exit. Every number printed
# is measured in this run, and the script exits non-zero if any claim fails.

set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
here=$root/examples/foresight

# resolve NAME VALUE — a path from the caller's directory, or a bare name from
# PATH (where `cargo install` puts traza-server and seed), as an absolute path.
resolve() {
  case $2 in
  /*) found=$2 ;;
  */*) dir=$(CDPATH= cd -- "$(dirname -- "$2")" 2>/dev/null && pwd) &&
    found=$dir/$(basename -- "$2") || found= ;;
  *) found=$(command -v "$2" 2>/dev/null) || found= ;;
  esac
  case $found in /*) ;; *) found= ;; esac # a builtin or an alias is no binary
  [ -n "$found" ] && [ -f "$found" ] && [ -x "$found" ] || {
    echo "$1=$2 is not an executable (a path, or a command on PATH)" >&2
    exit 1
  }
  printf '%s\n' "$found"
}

# TRAZA_BIN is the caller's, so it resolves before the cd below. The corpus is
# written by the seed binary built beside it, unless TRAZA_SEED_BIN names one.
server=./target/release/traza-server
seed=./target/release/seed
if [ -n "${TRAZA_BIN:-}" ]; then
  server=$(resolve TRAZA_BIN "$TRAZA_BIN")
  seed=$(dirname -- "$server")/seed
fi
if [ -n "${TRAZA_SEED_BIN:-}" ]; then
  seed=$(resolve TRAZA_SEED_BIN "$TRAZA_SEED_BIN")
fi
cd "$root"

# Functions, not commands kept in strings: expanding a string would split
# "$here" wherever the checkout's path has a space.
agent() { python3 "$here/agent.py" "$@"; }
render() { python3 "$here/render.py" "$@"; }
tmp=$(mktemp -d "${TMPDIR:-/tmp}/traza-foresight.XXXXXX")
port=${TRAZA_DEMO_PORT:-8132}
url=http://127.0.0.1:$port
pid=""

cleanup() {
  [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  rm -rf "$tmp"
}
trap cleanup EXIT
trap 'printf "\ninterrupted\n" >&2; exit 130' INT
trap 'exit 143' TERM

command -v python3 >/dev/null 2>&1 || {
  echo "this demo's agent, and its JSON reading, are python3" >&2
  exit 1
}
command -v curl >/dev/null 2>&1 || {
  echo "this demo drives the server with curl" >&2
  exit 1
}

bold() { printf '\n\033[1m%s\033[0m\n' "$*"; }
dim() { printf '\033[2m%s\033[0m\n' "$*"; }
fail() { printf '\nFAILED: %s\n' "$*" >&2; exit 1; }

# Process state, not kill -0: a child this shell has not reaped yet is a
# zombie, kill -0 succeeds on it, and dash (Linux /bin/sh) reaps only at wait
# points. Gone or Z means exited. Slim container images ship no ps; there
# kill -0 stands in, and a dead child that passes for alive costs only the
# wait until the startup loop's timeout, never a hang or a wrong diagnosis.
alive() {
  if ! command -v ps >/dev/null 2>&1; then
    kill -0 "$1" 2>/dev/null
    return
  fi
  case $(ps -o stat= -p "$1" 2>/dev/null | tr -d ' ') in '' | Z*) return 1 ;; esac
}

# post PATH BODY OUT / get PATH OUT — the status code is the output.
post() {
  curl -sS -o "$3" -w '%{http_code}' -X POST -H "Authorization: Bearer $token" \
    -H 'Content-Type: application/json' --data-binary "$2" "$url$1"
}
get() {
  curl -sS -o "$2" -w '%{http_code}' -H "Authorization: Bearer $token" "$url$1"
}
need() { # need GOT WANTED WHAT
  [ "$1" = "$2" ] || fail "$3 — got $1, wanted $2"
}

# ------------------------------------------------------------------ setup

# Whatever is missing is built; a TRAZA_BIN or TRAZA_SEED_BIN was checked above.
missing=""
[ -x "$server" ] || missing="$missing --bin traza-server"
if [ ! -x "$seed" ]; then
  [ -z "${TRAZA_BIN:-}" ] ||
    fail "no seed binary beside TRAZA_BIN, at $seed: build it there, or set TRAZA_SEED_BIN"
  missing="$missing --bin seed"
fi
if [ -n "$missing" ]; then
  echo "building$missing…"
  # Unquoted on purpose: each --bin is its own word.
  cargo build --release $missing
fi

# A data directory has one writer: the corpus goes in before the server opens it.
mkdir -p "$tmp/data"
"$seed" --data-dir "$tmp/data" --behaviours 2>"$tmp/seed.log" >/dev/null ||
  { sed 's/^/  /' "$tmp/seed.log" >&2; fail "seed could not write the behaviour corpus"; }

token=$(python3 -c 'import secrets; print(secrets.token_hex(16))')
TRAZA_TOKENS="rw:$token" "$server" --data-dir "$tmp/data" --port "$port" \
  >"$tmp/server.log" 2>&1 &
pid=$!
tries=0
# The server prints "listening on" only after its own bind succeeded, so a
# stranger already squatting on the port cannot pass for it.
until grep -q 'listening on' "$tmp/server.log" 2>/dev/null &&
  curl -sS -o /dev/null "$url/healthz" 2>/dev/null; do
  alive "$pid" || {
    sed 's/^/  /' "$tmp/server.log" >&2
    fail "the server exited during startup — is port $port taken? (set TRAZA_DEMO_PORT)"
  }
  tries=$((tries + 1))
  [ "$tries" -lt 40 ] || {
    sed 's/^/  /' "$tmp/server.log" >&2
    fail "the server did not come up on port $port (set TRAZA_DEMO_PORT)"
  }
  sleep 0.25
done
# And it must be the server holding this run's token: a stranger cannot know it.
need "$(get /v1/whoami /dev/null)" 200 "the server on port $port did not accept this run's token"

cat <<BANNER

┌──────────────────────────────────────────────────────────────────────┐
│  Traza foresight demo — a run's fate, read from every run before it  │
└──────────────────────────────────────────────────────────────────────┘

  server:   $url   (throwaway store)
  corpus:   seed --behaviours — runs whose outcomes are known exactly
  agent:    agent.py — a scripted support agent and a real harness: it
            asks POST /v1/fuses/check before every step and exports each
            step's span after it
BANNER

# ---------------------------------------------------------- 1 · history

bold "▸ 1 · the history"
dim "  Every completed run of an agent is one path through its steps. The"
dim "  cohorts: runs grouped by agent, with how each ended."
echo
render seeded "$tmp/seed.log"
code=$(get /v1/fate/cohorts "$tmp/cohorts.json")
need "$code" 200 "listing the cohorts"
render cohorts "$tmp/cohorts.json"

# ----------------------------------------------------------- 2 · the map

bold "▸ 2 · the Fate Map"
dim "  GET /v1/fate/map?agent=support-agent — every path the agent's runs took,"
dim "  how often each ended well, and where the paths that fail part from the"
dim "  ones that do not. Counting, not a model."
echo
code=$(curl -sS -o "$tmp/map.json" -w '%{http_code} %{time_total}' \
  -H "Authorization: Bearer $token" "$url/v1/fate/map?agent=support-agent")
set -- $code
need "$1" 200 "building the Fate Map"
render map "$tmp/map.json"
python3 -c 'import sys; print("    built in %.0f ms over HTTP, then cached for the fuses" % (1000 * float(sys.argv[1])))' "$2"

# ------------------------------------------------- 3 · a run goes wrong

bold "▸ 3 · a new run takes the doomed path"
dim "  ticket-1042: the order search comes back empty, and the agent runs it"
dim "  again. No policy acts on foresight yet — the agent only asks, after each"
dim "  step, GET /v1/fate/runs/ticket-1042: how did runs on this path end?"
echo
rc=0
agent run "$url" "$token" ticket-1042 doomed "$tmp/doomed-state.json" "$tmp/doomed-" 3 || rc=$?
need "$rc" 4 "the doomed agent should pause after three steps"
render doomed "$tmp/doomed-steps.jsonl" ticket-1042

# ---------------------------------------------------- 4 · a fuse acts

bold "▸ 4 · a predictive fuse stops it before the next search"
dim "  A policy for support-agent runs: stop a run once history is 95% sure that"
dim "  runs on its path fail more often than they succeed (min_success_odds 0.5)."
dim "  The agent resumes; its pre-call check, naming the next tool, is refused."
echo
code=$(post /v1/fuses/policies '{"name": "support", "match": {"service": "support-agent"},
  "limits": {"min_success_odds": 0.5}, "action": "stop"}' "$tmp/policy.json")
need "$code" 200 "creating the predictive policy"
agent ready "$url" "$token" ticket-1042 || fail "the fuse never saw a cached Fate Map"
rc=0
agent run "$url" "$token" ticket-1042 doomed "$tmp/doomed-state.json" "$tmp/doomed-" || rc=$?
need "$rc" 3 "the doomed agent should be stopped at the pre-call check"
render stopped "$tmp/doomed-check.json" "$tmp/doomed-steps.jsonl" "$tmp/map.json" ticket-1042

# -------------------------------------------- 5 · the healthy path runs

bold "▸ 5 · the same fuse lets a healthy run through"
dim "  ticket-1043 finds the order, refunds it once and answers. Its checks"
dim "  carry the run's foresight too; none falls below the limit."
echo
agent run "$url" "$token" ticket-1043 healthy "$tmp/healthy-state.json" "$tmp/healthy-" ||
  fail "the healthy agent should finish"
code=$(get /v1/fate/runs/ticket-1043 "$tmp/healthy-fate.json")
need "$code" 200 "reading the finished run's foresight"
render healthy "$tmp/healthy-steps.jsonl" "$tmp/healthy-fate.json" ticket-1043

# ----------------------------------------------------- 6 · calibration

bold "▸ 6 · how good is the map?"
dim "  Every map scores its own foresight: each completed run predicted at"
dim "  several depths, from the half of the cohort it is not in, and checked"
dim "  against how it ended. Read skill before trusting a verdict."
echo
render calibration "$tmp/map.json"

cat <<'CLOSING'

┌──────────────────────────────────────────────────────────────────────┐
│  Done — the store and its directory are removed on exit.             │
│                                                                      │
│  What was shown: the Fate Map is arithmetic over stored runs — paths │
│  counted, rates with Wilson intervals, forks held to a Fisher test.  │
│  Foresight walks a live run's path down it; a predictive fuse acts   │
│  on the interval's upper bound, at the pre-call check, before the    │
│  next step is paid for — and lifts by itself if the path recovers.   │
│  What it does not show, because it is not true: cause. A fatal fork  │
│  says where runs went wrong, not why; example_runs give you a pair   │
│  to diff.                                                            │
│                                                                      │
│  Reference: docs/guide/foresight.md                                  │
└──────────────────────────────────────────────────────────────────────┘
CLOSING
