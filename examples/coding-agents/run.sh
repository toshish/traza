#!/bin/sh
# Claude Code and Codex sessions, recorded from their transcripts and read back.
#
# Synthetic transcripts — four sessions on one small Python shop, authored in
# the shapes both tools write to disk — are imported with
# `traza-server import`, no SDK and no proxy. Then: what each agent and
# sub-agent cost; the cache autopsy naming an expired TTL and a /compact;
# an X-Ray of the call before the compaction and the one after, which lost
# the instruction "never touch migrations/"; the detectors finding the
# session that edited a test to make it pass, and a TQL query finding the
# migration written after the compaction; blame naming the session, step and
# prompt behind three lines of code; and `--watch` picking up lines appended
# to a live transcript.
#
#   examples/coding-agents/run.sh
#
# It builds what it needs (or uses TRAZA_BIN), works in a throwaway
# directory, and cleans up on exit. No real transcript is read. Every number
# printed is measured in this run, and the script exits non-zero if any
# claim fails.

set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
here=$root/examples/coding-agents

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
# Blame runs from inside the project, so the server's path must not depend
# on the directory it is run from.
case $server in /*) ;; *) server=$root/${server#./} ;; esac

# Functions, not commands kept in strings: expanding a string would split
# "$here" wherever the checkout's path has a space.
transcripts() { python3 "$here/transcripts.py" "$@"; }
render() { python3 "$here/render.py" "$@"; }
field() { python3 -c 'import json, sys
value = json.load(open(sys.argv[1]))
for key in sys.argv[2:]:
    value = value[int(key)] if isinstance(value, list) else value[key]
print(value)' "$@"; }
tmp=$(mktemp -d "${TMPDIR:-/tmp}/traza-coding-agents.XXXXXX")
port=${TRAZA_DEMO_PORT:-8133}
url=http://127.0.0.1:$port
pid=""
watch_pid=""

cleanup() {
  [ -n "$watch_pid" ] && kill "$watch_pid" 2>/dev/null || true
  [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  rm -rf "$tmp"
}
trap cleanup EXIT
trap 'printf "\ninterrupted\n" >&2; exit 130' INT
trap 'exit 143' TERM

command -v python3 >/dev/null 2>&1 || {
  echo "this demo's transcripts, and its JSON reading, are python3" >&2
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

get() {
  curl -sS -o "$2" -w '%{http_code}' -H "Authorization: Bearer $token" "$url$1"
}
need() { # need GOT WANTED WHAT
  [ "$1" = "$2" ] || fail "$3 — got $1, wanted $2"
}
now() { python3 -c 'import time; print("%.6f" % time.time())'; }
since() { python3 -c 'import sys, time; print("%.2f" % (time.time() - float(sys.argv[1])))' "$1"; }

# ------------------------------------------------------------------ setup

if [ ! -x "$server" ]; then
  echo "building traza-server…"
  cargo build --release --bin traza-server
fi

# The rates this demo prices with, cache rates included: without them a
# Claude Code session's cache traffic goes unpriced (docs/configuration.md).
# They are the vendors' list prices on 2026-10-18 (cache writes at the
# five-minute rate). Haiku 5.5 costs more for prompts over 100K tokens; the
# demo's Haiku calls stay near 11K, so its base rate is the one that applies.
cat >"$tmp/pricing.json" <<'EOF'
{"models": {
  "claude-opus-5-5":  {"input_per_mtok": 4.00, "output_per_mtok": 20.00,
                       "cache_read_per_mtok": 0.20, "cache_write_per_mtok": 5.00},
  "claude-haiku-5-5": {"input_per_mtok": 0.10, "output_per_mtok": 0.50,
                       "cache_read_per_mtok": 0.01, "cache_write_per_mtok": 0.125},
  "gpt-6.1-sol":      {"input_per_mtok": 2.00, "output_per_mtok": 10.00,
                       "cache_read_per_mtok": 0.10, "cache_write_per_mtok": 2.50}
}}
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

# The importer and blame read the token from the environment, never argv.
export TRAZA_TOKEN="$token"

transcripts write "$tmp" >/dev/null
manifest=$tmp/manifest.json
project=$(field "$manifest" project)
claude_dir=$(field "$manifest" claude_code path)
codex_dir=$(field "$manifest" codex path)
coupons=$(field "$manifest" sessions coupons)
tests=$(field "$manifest" sessions tests)
rename=$(field "$manifest" sessions rename)
endpoint=$(field "$manifest" sessions endpoint)

cat <<BANNER

┌──────────────────────────────────────────────────────────────────────┐
│  Traza coding-agents demo — Claude Code and Codex, zero setup        │
└──────────────────────────────────────────────────────────────────────┘

  server:       $url   (--pricing with cache rates; throwaway store)
  transcripts:  transcripts.py — four synthetic sessions on one small
                Python shop, in the shapes Claude Code and Codex write to
                ~/.claude/projects and ~/.codex/sessions. No real session.
  sessions:     coupons  (Claude Code, an Explore and a code-reviewer
                          sub-agent, a 16-minute pause, a /compact)
                tests    (Claude Code: asked to fix a failing test)
                rename   (Claude Code: a rename, finished without tests)
                endpoint (Codex: a new API route via apply_patch)
BANNER

# ---------------------------------------------------------- 1 · import

bold "▸ 1 · import the transcripts"
dim "  traza-server import claude-code and import codex, pointed at the"
dim "  synthetic ~/.claude/projects and ~/.codex/sessions."
echo
started=$(now)
"$server" import claude-code --path "$claude_dir" --url "$url" \
  --state "$tmp/import-state.json" --json >"$tmp/import-claude.json" ||
  fail "importing the Claude Code transcripts"
"$server" import codex --path "$codex_dir" --url "$url" \
  --state "$tmp/import-state.json" --json >"$tmp/import-codex.json" ||
  fail "importing the Codex rollout"
render imported "$manifest" "$tmp/import-claude.json" "$tmp/import-codex.json" "$(since "$started")"
"$server" import claude-code --path "$claude_dir" --url "$url" \
  --state "$tmp/import-state.json" --json >"$tmp/reimport.json" || fail "importing again"
render reimported "$tmp/reimport.json"

# ------------------------------------------------------------ 2 · cost

bold "▸ 2 · what each agent cost"
dim "  GET /v1/runs/{session}/topology for each session: its agents — the main"
dim "  loop and every sub-agent — with their calls and cost, priced by the server."
echo
for label in coupons tests rename endpoint; do
  eval "session=\$$label"
  code=$(get "/v1/runs/$session/topology" "$tmp/topology-$label.json")
  need "$code" 200 "reading the $label session's topology"
done
render cost "$manifest" "$tmp/pricing.json" "$tmp/topology-coupons.json" "$tmp/topology-tests.json" \
  "$tmp/topology-rename.json" "$tmp/topology-endpoint.json"

# --------------------------------------------------- 3 · cache autopsy

bold "▸ 3 · the cache autopsy of the coupons session"
dim "  GET /v1/cache/{session}: every model call compared with the previous call"
dim "  of its conversation — what broke the prompt cache, and what it cost."
echo
code=$(get "/v1/cache/$coupons" "$tmp/cache.json")
need "$code" 200 "running the cache autopsy"
code=$(get "/v1/tapes/$coupons" "$tmp/tape-coupons.json")
need "$code" 200 "reading the coupons session's tape"
render autopsy "$manifest" "$tmp/cache.json" "$tmp/tape-coupons.json"

# ------------------------------------------------------------ 4 · X-Ray

bold "▸ 4 · X-Ray: what the model saw, before and after the /compact"
dim "  GET /v1/xray/{trace}/{span}: one call's context rebuilt from the"
dim "  transcript's deltas, each chunk traced to the step that introduced it."
echo
# Captured first: `set -- $(…)` would swallow the render's failure.
ids=$(render compacted-step "$tmp/cache.json") ||
  fail "the autopsy found no rewritten (compacted) call to X-ray"
set -- $ids
compact_step=$1 trace=$3 after_span=$4 before_span=$5
code=$(get "/v1/xray/$trace/$before_span" "$tmp/xray-before.json")
need "$code" 200 "X-raying the call before the compaction"
code=$(get "/v1/xray/$trace/$after_span" "$tmp/xray-after.json")
need "$code" 200 "X-raying the call after the compaction"
render xray "$manifest" "$tmp/xray-before.json" "$tmp/xray-after.json"

# ------------------------------------------------------------- 5 · TQL

bold "▸ 5 · behaviour: the detectors, and one query of our own"
dim "  The detector library is a set of named TQL patterns, run over the"
dim "  window's sessions; a pattern of your own is one GET."
echo
code=$(get /v1/detectors/findings "$tmp/findings.json")
need "$code" 200 "running the detectors"
code=$(get "/v1/tapes/$tests" "$tmp/tape-tests.json")
need "$code" 200 "reading the tests session's tape"
render findings "$manifest" "$tmp/findings.json" "$tmp/tape-tests.json"
echo
code=$(curl -sS -G -o "$tmp/search.json" -w '%{http_code}' -H "Authorization: Bearer $token" \
  --data-urlencode 'q=edit[file~"(^|/)migrations/"]' "$url/v1/trajectories/search")
need "$code" 200 "searching for edits under migrations/"
render search "$manifest" "$tmp/search.json" "$compact_step"

# ----------------------------------------------------------- 6 · blame

bold "▸ 6 · blame: which session, step and prompt wrote this line?"
dim "  traza-server blame FILE:LINE, run inside the project — the CLI reads the"
dim "  line from the file and asks GET /v1/blame."
echo
for index in 0 1 2; do
  file=$(field "$manifest" blame "$index" file)
  line=$(field "$manifest" blame "$index" line)
  rc=0
  (cd "$project" && "$server" blame "$file:$line" --url "$url" --json) >"$tmp/blame-$index.json" || rc=$?
  render blame "$manifest" "$index" "$tmp/blame-$index.json" "$rc"
done

# ----------------------------------------------------------- 7 · watch

bold "▸ 7 · --watch: a live transcript, followed as it is written"
dim "  The rename session resumes: its transcript grows by a prompt and a tool"
dim "  call, then the result and an answer, while an import --watch follows it."
echo
"$server" import claude-code --path "$claude_dir" --url "$url" --state "$tmp/import-state.json" \
  --watch --interval-ms 200 >"$tmp/watch.log" 2>&1 &
watch_pid=$!
tries=0
until grep -q 'watching for changes' "$tmp/watch.log" 2>/dev/null; do
  alive "$watch_pid" || { sed 's/^/  /' "$tmp/watch.log" >&2; fail "the watch exited"; }
  tries=$((tries + 1))
  [ "$tries" -lt 100 ] || fail "the watch never finished its first pass"
  sleep 0.1
done
echo "    import claude-code --watch --interval-ms 200: first pass done, nothing new to send"
code=$(get "/v1/tapes/$rename" "$tmp/tape-rename.json")
need "$code" 200 "reading the rename session's tape"
steps=$(python3 -c 'import json, sys; print(len(json.load(open(sys.argv[1]))["steps"]))' "$tmp/tape-rename.json")
echo
wrote=$(transcripts append "$tmp" 1)
echo "    appended: the prompt \"$(field "$manifest" watch_prompt)\" and a Bash call (pytest -q)"
render arrived "$url" "$token" "$rename" $((steps + 2)) "$wrote" "$tmp/tape-rename.json"
render live "$url" "$token" "$rename" Bash
echo
wrote=$(transcripts append "$tmp" 2)
echo "    appended: the tool result (4 passed) and the answer"
render arrived "$url" "$token" "$rename" $((steps + 3)) "$wrote" "$tmp/tape-rename.json"
code=$(get '/v1/detectors/findings?detector=unverified_finish' "$tmp/findings-after.json")
need "$code" 200 "re-running the unverified_finish detector"
render rechecked "$manifest" "$tmp/findings.json" "$tmp/findings-after.json"
kill "$watch_pid" 2>/dev/null || true
wait "$watch_pid" 2>/dev/null || true
watch_pid=""

cat <<'CLOSING'

┌──────────────────────────────────────────────────────────────────────┐
│  Done — the store, the transcripts and the project are removed.      │
│                                                                      │
│  What was shown: a coding agent's transcript is already a complete   │
│  record. Imported, every session is a run — priced per agent, its    │
│  cache breaks named and costed, each call's context rebuilt with     │
│  where every chunk came from, its behaviour matched like a regex,    │
│  each written line traced to a session, step and prompt — and a      │
│  live session follows within a poll. What it cannot show: what a     │
│  transcript never recorded — the system prompt, the tool catalog,    │
│  lines written by shell commands.                                    │
│                                                                      │
│  Reference: docs/guide/recorders.md                                  │
└──────────────────────────────────────────────────────────────────────┘
CLOSING
