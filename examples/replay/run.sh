#!/bin/sh
# Record an agent run, then replay it with zero model calls.
#
# An agent on the OpenAI chat-completions wire answers a question with one
# tool call, against a stand-in model, and records the run into Traza the way
# OpenTelemetry GenAI instrumentation does. Then the model is switched off,
# and the same agent code — pointed at a Traza replay through OPENAI_BASE_URL
# — gets every answer back identically. A changed prompt is refused in strict
# mode, naming the step; and GET /v1/diff names where a changed run parted
# from the original.
#
#   examples/replay/run.sh
#
# It builds what it needs (or uses TRAZA_BIN), works in a throwaway
# directory, and cleans up on exit. Every number printed is measured in this
# run, and the script exits non-zero if any claim fails.

set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
here=$root/examples/replay

# TRAZA_BIN is the caller's, so it resolves before the cd below: a path from
# the caller's directory, a bare name from PATH (where `cargo install` puts
# traza-server).
server=./target/release/traza-server
if [ -n "${TRAZA_BIN:-}" ]; then
  case $TRAZA_BIN in
  /*) server=$TRAZA_BIN ;;
  */*) dir=$(CDPATH= cd -- "$(dirname -- "$TRAZA_BIN")" 2>/dev/null && pwd) &&
    server=$dir/$(basename -- "$TRAZA_BIN") || server= ;;
  *) server=$(command -v "$TRAZA_BIN" 2>/dev/null) || server= ;;
  esac
  case $server in /*) ;; *) server= ;; esac # a builtin or an alias is no binary
  [ -n "$server" ] && [ -f "$server" ] && [ -x "$server" ] || {
    echo "TRAZA_BIN=$TRAZA_BIN is not an executable (a path, or a command on PATH)" >&2
    exit 1
  }
fi
cd "$root"

# A function, not a command kept in a string: expanding the string would
# split "$here" wherever the checkout's path has a space.
render() { python3 "$here/render.py" "$@"; }
tmp=$(mktemp -d "${TMPDIR:-/tmp}/traza-replay.XXXXXX")
port=${TRAZA_DEMO_PORT:-8130}
url=http://127.0.0.1:$port
pid=""
model_pid=""

cleanup() {
  [ -n "$model_pid" ] && kill "$model_pid" 2>/dev/null || true
  [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  rm -rf "$tmp"
}
trap cleanup EXIT
trap 'printf "\ninterrupted\n" >&2; exit 130' INT
trap 'exit 143' TERM

command -v python3 >/dev/null 2>&1 || {
  echo "this demo's agent, stand-in model and JSON reading are python3" >&2
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

# get PATH OUT / post PATH BODY OUT, with the read-only token: everything a
# replay needs is a read. The status code is the output.
get() {
  curl -sS -o "$2" -w '%{http_code}' -H "Authorization: Bearer $ro" "$url$1"
}
post() {
  curl -sS -o "$3" -w '%{http_code}' -X POST -H "Authorization: Bearer $ro" \
    -H 'Content-Type: application/json' --data-binary "$2" "$url$1"
}
need() { # need GOT WANTED WHAT
  [ "$1" = "$2" ] || fail "$3 — got $1, wanted $2"
}

start_model() {
  rm -f "$tmp/model.port"
  python3 "$here/model.py" "$tmp/model.port" "$tmp/model.count" &
  model_pid=$!
  tries=0
  until [ -s "$tmp/model.port" ]; do
    alive "$model_pid" || fail "the stand-in model exited during startup"
    tries=$((tries + 1))
    [ "$tries" -lt 100 ] || fail "the stand-in model did not come up"
    sleep 0.1
  done
  model_url=http://127.0.0.1:$(cat "$tmp/model.port")/v1
}

stop_model() {
  kill "$model_pid"
  wait "$model_pid" 2>/dev/null || true
  model_pid=""
}

# ------------------------------------------------------------------ setup

if [ ! -x "$server" ]; then
  echo "building traza-server…"
  cargo build --release --bin traza-server
fi

rw=$(python3 -c 'import secrets; print(secrets.token_hex(16))')
ro=$(python3 -c 'import secrets; print(secrets.token_hex(16))')
mkdir -p "$tmp/data"
TRAZA_TOKENS="rw:$rw,ro:$ro" "$server" --data-dir "$tmp/data" --port "$port" \
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
# And it must be the server holding this run's tokens: a stranger cannot know them.
need "$(get '/v1/spans?limit=1' /dev/null)" 200 "the server on port $port did not accept this run's token"

# The agent's telemetry goes in with the rw token; everything else here is ro.
export TRAZA_URL=$url TRAZA_TOKEN=$rw

cat <<BANNER

┌──────────────────────────────────────────────────────────────────────┐
│  Traza replay demo — record a run, replay it with zero model calls   │
└──────────────────────────────────────────────────────────────────────┘

  server:  $url   (tokens: rw for the agent's telemetry, ro for the rest)
  agent:   agent.py — stdlib urllib on the OpenAI chat-completions wire,
           configured by OPENAI_BASE_URL and OPENAI_API_KEY, recording
           OTel GenAI spans (gen_ai.input/output.messages) as it runs
  model:   model.py — a scripted stand-in provider on an ephemeral port
BANNER

# ---------------------------------------------------------- 1 · record

bold "▸ 1 · record a run"
dim "  The agent asks the model about Paris; the model asks for get_weather; the"
dim "  agent runs it and sends the result back; the model answers."
echo
start_model
OPENAI_BASE_URL=$model_url OPENAI_API_KEY=standin-key TRAZA_RUN=weather-1 \
  python3 "$here/agent.py" "$tmp/recorded.json" || fail "the recording run failed"
render transcript "$tmp/recorded.json"
recorded_calls=$(render count "$tmp/model.count")
need "$recorded_calls" 2 "the stand-in model should have answered two calls"
echo
echo "  the run's tape, GET /v1/tapes/weather-1:"
code=$(get /v1/tapes/weather-1 "$tmp/tape.json")
need "$code" 200 "reading the tape"
render tape "$tmp/tape.json"

# ---------------------------------------------------------- 2 · replay

bold "▸ 2 · switch the model off; replay the run"
dim "  The stand-in model is stopped. POST /v1/replays snapshots the tape, and"
dim "  the same agent code runs again with OPENAI_BASE_URL set to the replay and"
dim "  the read-only Traza token as its OPENAI_API_KEY."
echo
stop_model
if curl -sS -o /dev/null --max-time 2 -X POST "$model_url/chat/completions" 2>/dev/null; then
  fail "the stand-in model still answers at $model_url"
fi
echo "    stand-in model stopped: $model_url now refuses connections (checked)"
code=$(post /v1/replays '{"run": "weather-1"}' "$tmp/replay.json")
need "$code" 201 "creating the replay"
render created "$tmp/replay.json"
replay_id=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["id"])' "$tmp/replay.json")
base=$url/v1/replays/$replay_id/openai/v1
echo
OPENAI_BASE_URL=$base OPENAI_API_KEY=$ro TRAZA_RUN=weather-1-replay \
  python3 "$here/agent.py" "$tmp/replayed.json" || fail "the replayed run failed"
render same "$tmp/recorded.json" "$tmp/replayed.json" "$base"
code=$(get "/v1/replays/$replay_id" "$tmp/status.json")
need "$code" 200 "reading the replay's status"
echo
echo "  GET /v1/replays/$replay_id:"
render served "$tmp/status.json" 2
after_calls=$(render count "$tmp/model.count")
need "$after_calls" "$recorded_calls" "the stand-in model must not have been called during the replay"
echo "    stand-in model calls: $recorded_calls while recording, $after_calls after the replay — zero"
echo "    model calls were made; both answers came from the tape (asserted)"

# ---------------------------------------------------- 3 · strict refusal

bold "▸ 3 · change the prompt; strict mode refuses"
dim "  A fresh replay of the same run, and the agent with its v2 system prompt"
dim "  (temperatures in Fahrenheit). The recorded answer was to a different"
dim "  question, so serving it would be wrong — and it is not served."
echo
code=$(post /v1/replays '{"run": "weather-1"}' "$tmp/replay2.json")
need "$code" 201 "creating the second replay"
replay2=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["id"])' "$tmp/replay2.json")
rc=0
WEATHER_PROMPT=v2 OPENAI_BASE_URL=$url/v1/replays/$replay2/openai/v1 OPENAI_API_KEY=$ro \
  python3 "$here/agent.py" "$tmp/refused.json" || rc=$?
need "$rc" 4 "the changed agent should be refused"
code=$(get "/v1/replays/$replay2" "$tmp/status2.json")
need "$code" 200 "reading the second replay's status"
render refused "$tmp/refused.json" "$tmp/status2.json"

# ------------------------------------------------------------ 4 · diff

bold "▸ 4 · what changed, step by step"
dim "  The model is back, the v2 prompt runs for real and is recorded as"
dim "  weather-2. GET /v1/diff aligns it with the original — and, as a control,"
dim "  the replayed run (recorded as it ran in beat 2) with the original."
echo
start_model
WEATHER_PROMPT=v2 OPENAI_BASE_URL=$model_url OPENAI_API_KEY=standin-key TRAZA_RUN=weather-2 \
  python3 "$here/agent.py" "$tmp/v2.json" || fail "the v2 run failed"
stop_model
render transcript "$tmp/v2.json"
echo
echo "  GET /v1/diff?a=weather-1&b=weather-1-replay"
code=$(get '/v1/diff?a=weather-1&b=weather-1-replay' "$tmp/diff-replay.json")
need "$code" 200 "diffing the replayed run"
render diff "$tmp/diff-replay.json" same
echo
echo "  GET /v1/diff?a=weather-1&b=weather-2"
code=$(get '/v1/diff?a=weather-1&b=weather-2' "$tmp/diff-v2.json")
need "$code" 200 "diffing the changed run"
render diff "$tmp/diff-v2.json" changed

cat <<'CLOSING'

┌──────────────────────────────────────────────────────────────────────┐
│  Done — the store and its directory are removed on exit.             │
│                                                                      │
│  What was shown: a recorded run replays through the agent's own      │
│  code with every model answer served from the tape — no model, no    │
│  key, no network beyond Traza. Strict mode refuses a request that    │
│  is not the recorded one rather than answer the wrong question. The  │
│  tool ran live each time: a replay answers model calls, not tool     │
│  calls (POST /v1/replays/{id}/tools/next serves recorded results to  │
│  a harness that wants them).                                         │
│                                                                      │
│  Reference: docs/guide/replay.md                                     │
└──────────────────────────────────────────────────────────────────────┘
CLOSING
