#!/bin/sh
# A runaway agent, stopped between steps.
#
# A research agent loops on a failing tool. A run-fuse policy stops it: the
# ingest acknowledgement says so, the verdict names the reason and points at
# the evidence, and the pre-call check refuses the next step before it is
# taken. Then the kill switch stops a second, healthy run; a reset is shown
# not to refill a budget; a grant approves the first run to finish; and the
# Claude Code hook pack (examples/claude-code) relays a person's note to a
# session and refuses its next tool call once it loops.
#
#   examples/guard/run.sh
#
# It builds what it needs (or uses TRAZA_BIN), works in a throwaway
# directory, and cleans up on exit. Every number printed is measured in this
# run, and the script exits non-zero if any claim fails.

set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
here=$root/examples/guard

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
agent() { python3 "$here/agent.py" "$@"; }
hook=$root/examples/claude-code/traza_hook.py
tmp=$(mktemp -d "${TMPDIR:-/tmp}/traza-guard.XXXXXX")
port=${TRAZA_DEMO_PORT:-8129}
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

# post PATH BODY OUT — the status code is the output, the body lands in OUT.
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

if [ ! -x "$server" ]; then
  echo "building traza-server…"
  cargo build --release --bin traza-server
fi

# The pricing file is the server's, and the agent prices its own estimates
# from the same one: the two must agree to the cent, and beat 3 checks it.
cat >"$tmp/pricing.json" <<'EOF'
{"models": {"sim-large": {"input_per_mtok": 3.00, "output_per_mtok": 15.00}}}
EOF

token=$(python3 -c 'import secrets; print(secrets.token_hex(16))')
mkdir -p "$tmp/data"
TRAZA_TOKENS="rw:$token" "$server" --data-dir "$tmp/data" --port "$port" \
  --pricing "$tmp/pricing.json" >"$tmp/server.log" 2>&1 &
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
│  Traza guard demo — a runaway agent, stopped between steps           │
└──────────────────────────────────────────────────────────────────────┘

  server:   $url   (--pricing below; throwaway store)
  pricing:  sim-large at \$3.00 / \$15.00 per million input / output tokens
  agent:    agent.py — a scripted model and a real harness: it asks
            POST /v1/fuses/check before every step and reads every
            ingest acknowledgement after one
BANNER

# --------------------------------------------------------- 1 · the policy

bold "▸ 1 · the policy"
dim "  Runs of research-agent stop at 4 identical tool calls or \$0.035 spent,"
dim "  whichever comes first. Cost is priced by the server from --pricing."
echo
code=$(post /v1/fuses/policies '{"name": "research", "match": {"service": "research-agent"},
  "limits": {"max_repeats": 4, "max_cost_usd": 0.035}, "action": "stop"}' "$tmp/policy.json")
need "$code" 200 "creating the policy"
agent policy "$tmp/policy.json"

# ----------------------------------------------------------- 2 · the loop

bold "▸ 2 · the loop, and the acknowledgement that ends it"
dim "  The page it needs answers 503 every time, and the agent retries with the"
dim "  same arguments — the shape of every runaway loop. Each failure goes back"
dim "  into the prompt, so each attempt costs more than the last."
echo
rc=0
agent agent "$url" "$token" sess-loop summarise "$tmp/loop.json" "$tmp/pricing.json" \
  down 99 "$tmp/loop-" || rc=$?
need "$rc" 3 "the looping agent should be halted by Traza (exit 3)"
agent stopped-on-ack "$tmp/loop-ack.json" "$tmp/loop.json" sess-loop 4
echo
dim "  The fourth identical call crossed the limit. Its spans were stored — a"
dim "  verdict is never a refusal of the batch it rides on — and the agent heard"
dim "  'stop' before it started a fifth step."

# -------------------------------------------------------- 3 · the verdict

bold "▸ 3 · the verdict, its reasons and its evidence"
dim "  GET /v1/fuses/runs/sess-loop — and each evidence span read back by id."
echo
code=$(get /v1/fuses/runs/sess-loop "$tmp/verdict.json")
need "$code" 200 "reading the run's verdict"
agent verdict "$tmp/verdict.json" "$tmp/loop.json" "$url" "$token" 4

# ----------------------------------------------------------- 4 · the gate

bold "▸ 4 · the gate refuses the next step"
dim "  The agent is started again on the same run, as a supervisor restarting a"
dim "  crashed worker would. Its first act is the pre-call check, and the check"
dim "  refuses: the stop is sticky until someone resets the run."
echo
before=$(agent field "$tmp/verdict.json" usage steps)
rc=0
agent agent "$url" "$token" sess-loop summarise "$tmp/loop.json" "$tmp/pricing.json" \
  down 99 "$tmp/loop-" || rc=$?
need "$rc" 3 "the restarted agent should be refused at the gate"
agent gate "$tmp/loop-check.json" sticky
code=$(get /v1/fuses/runs/sess-loop "$tmp/verdict2.json")
need "$code" 200 "re-reading the run"
after=$(agent field "$tmp/verdict2.json" usage steps)
need "$after" "$before" "the refused agent must not have recorded a step"
echo "    steps recorded: $before before the attempt, $after after — nothing ran (asserted)"

# ---------------------------------------------------- 5 · the kill switch

bold "▸ 5 · the kill switch, on a healthy run"
dim "  A second run makes progress — distinct calls, every one succeeding. An"
dim "  operator stops it between steps anyway: POST /v1/fuses/runs/{run}/trip."
echo
rc=0
agent agent "$url" "$token" sess-healthy research "$tmp/healthy.json" "$tmp/pricing.json" \
  up 2 "$tmp/healthy-" || rc=$?
need "$rc" 4 "the healthy agent should pause after two steps"
code=$(post /v1/fuses/runs/sess-healthy/trip '{"reason": "operator: hold for review of the research plan"}' \
  "$tmp/trip.json")
need "$code" 200 "pulling the kill switch"
echo "    trip →"
agent control "$tmp/trip.json" stop
echo
echo "    the agent resumes:"
rc=0
agent agent "$url" "$token" sess-healthy research "$tmp/healthy.json" "$tmp/pricing.json" \
  up 99 "$tmp/healthy-" || rc=$?
need "$rc" 3 "the killed run should be refused at the gate"
agent gate "$tmp/healthy-check.json" manual

# ------------------------------------------- 6 · reset, then a grant

bold "▸ 6 · a reset does not refill a budget; a grant does"
dim "  The upstream is fixed and the looping run is reset. Reset clears the stop"
dim "  and the repeat count — but not what the loop spent, so the next step's"
dim "  estimated cost is refused before it is paid for."
echo
code=$(post /v1/fuses/runs/sess-loop/reset '{}' "$tmp/reset.json")
need "$code" 200 "resetting the run"
echo "    reset →"
agent control "$tmp/reset.json" warn
echo
echo "    the agent resumes, the upstream now answering:"
rc=0
agent agent "$url" "$token" sess-loop summarise "$tmp/loop.json" "$tmp/pricing.json" \
  up 99 "$tmp/loop-" || rc=$?
need "$rc" 3 "the next step should be refused on cost"
agent gate "$tmp/loop-check.json" hypothetical
echo
dim "  Someone with authority approves the spend: a reset with a grant raises"
dim "  this one run's cost limit, over whatever its policy says."
echo
code=$(post /v1/fuses/runs/sess-loop/reset '{"grant": {"max_cost_usd": 0.10}}' "$tmp/grant.json")
need "$code" 200 "resetting with a grant"
echo "    reset with grant →"
agent control "$tmp/grant.json" continue max_cost_usd=0.1
echo
echo "    the agent resumes:"
agent agent "$url" "$token" sess-loop summarise "$tmp/loop.json" "$tmp/pricing.json" \
  up 99 "$tmp/loop-" || fail "the approved agent should finish its task"
code=$(get /v1/fuses/runs/sess-loop "$tmp/final.json")
need "$code" 200 "reading the finished run"
agent finished "$tmp/final.json" "$tmp/loop.json" 0.035 0.10

# -------------------------------------------------- 7 · the Claude Code hook

bold "▸ 7 · the same fuse in front of Claude Code"
dim "  examples/claude-code/traza_hook.py, fed the JSON Claude Code would pipe"
dim "  to it, recording tool calls itself (TRAZA_RECORD_TOOLS=1). Policy:"
dim "  sessions of service claude-code stop at 3 identical tool calls."
echo
code=$(post /v1/fuses/policies '{"name": "claude-code", "match": {"service": "claude-code"},
  "limits": {"max_repeats": 3}, "action": "stop"}' "$tmp/cc-policy.json")
need "$code" 200 "creating the claude-code policy"
session="cc-$(python3 -c 'import secrets; print(secrets.token_hex(4))')"
code=$(post "/v1/runs/$session/nudges" '{"message": "The suite is slow; run one test file at a time.",
  "kind": "advice"}' "$tmp/cc-nudge.json")
need "$code" 201 "queueing a nudge for the session"
hook_call() { # hook_call EVENT TOOL_USE_ID — exit code is the hook's
  printf '{"session_id": "%s", "hook_event_name": "%s", "tool_name": "Bash", "tool_use_id": "%s", "tool_input": {"command": "npm test"}}' \
    "$session" "$1" "$2" |
    TRAZA_URL=$url TRAZA_TOKEN=$token TRAZA_RECORD_TOOLS=1 python3 "$hook" \
      >"$tmp/hook.out" 2>"$tmp/hook.err"
}
rc=0
hook_call PreToolUse toolu_0 || rc=$?
printf '    %-12s %-14s %-24s → ' PreToolUse 'Bash npm test' '(a person left a note)'
agent hook "$rc" "$tmp/hook.out" context "run one test file at a time"
for n in 1 2 3; do
  rc=0
  hook_call PostToolUse "toolu_$n" || rc=$?
  printf '    %-12s %-14s %-24s → ' PostToolUse 'Bash npm test' "(records call $n of 3)"
  if [ "$n" -lt 3 ]; then
    agent hook "$rc" "$tmp/hook.out" none
  else
    agent hook "$rc" "$tmp/hook.out" block "max_repeats 3 reached" "already ran"
  fi
done
rc=0
hook_call PreToolUse toolu_4 || rc=$?
printf '    %-12s %-14s %-24s → ' PreToolUse 'Bash npm test' '(the next call)'
agent hook "$rc" "$tmp/hook.out" deny "Traza stopped run $session" "max_repeats 3 reached"
echo
dim "  permissionDecision \"deny\" is Claude Code's 'refuse this call': the reason"
dim "  above is what the model is shown, in place of the fourth test run."

# ------------------------------------------------------- 8 · the record

bold "▸ 8 · the record"
dim "  GET /v1/fuses/events: every transition above, in order, as an alert"
dim "  dispatcher polling the feed would have seen it."
echo
code=$(get /v1/fuses/events "$tmp/events.json")
need "$code" 200 "reading the event feed"
agent events "$tmp/events.json" "$session"

cat <<'CLOSING'

┌──────────────────────────────────────────────────────────────────────┐
│  Done — the store and its directory are removed on exit.             │
│                                                                      │
│  What was shown: a fuse is arithmetic over recorded spans. The ack   │
│  of the step that crossed a limit says stop; the pre-call check      │
│  refuses the next step before it runs; the kill switch stops a run   │
│  whatever its usage; a reset clears a stop but never refills a       │
│  budget; a grant raises one run's limit. Every verdict acts BETWEEN  │
│  steps — a step already running is never interrupted.                │
│                                                                      │
│  Reference: docs/guide/guard.md                                      │
└──────────────────────────────────────────────────────────────────────┘
CLOSING
