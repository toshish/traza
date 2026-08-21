#!/bin/sh
# A scripted agent investigation against Traza's MCP server.
#
# Everything below is what an MCP client actually sends: one JSON-RPC message
# per HTTP POST, no SDK. The point is the shape of the conversation — orient,
# find what is failing, open the trace that explains it, account for the cost —
# and what each tool gives back.
#
#   examples/mcp-demo/run.sh
#
# It builds what it needs, seeds a throwaway store, and cleans up on exit.

set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
here=$root/examples/mcp-demo
cd "$root"

server=./target/release/traza-server
seed=./target/release/seed
render="python3 $here/render.py"
data=$(mktemp -d "${TMPDIR:-/tmp}/traza-mcp-demo.XXXXXX")
port=${TRAZA_DEMO_PORT:-8123}
pid=""

cleanup() {
  [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  rm -rf "$data"
}
trap cleanup EXIT INT TERM

command -v python3 >/dev/null 2>&1 || {
  echo "this demo uses python3 only to pretty-print the JSON-RPC replies" >&2
  exit 1
}
command -v curl >/dev/null 2>&1 || {
  echo "this demo drives the server with curl" >&2
  exit 1
}

bold() { printf '\n\033[1m%s\033[0m\n' "$*"; }
dim() { printf '\033[2m%s\033[0m\n' "$*"; }

# POST one JSON-RPC message.
rpc() {
  curl -sS -X POST "http://127.0.0.1:$port/v1/mcp" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -d "$1"
}

# One tool call: show the request a client would send, then the result.
call() {
  bold "▸ $1"
  dim "  $3"
  dim "  → tools/call $1 $2"
  printf '\n'
  rpc "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"$1\",\"arguments\":$2}}" |
    $render tool
}

# ------------------------------------------------------------------ setup

if [ ! -x "$server" ] || [ ! -x "$seed" ]; then
  echo "building traza-server and seed…"
  cargo build --release --bin traza-server --bin seed
fi

echo "seeding a throwaway store in $data"
"$seed" --data-dir "$data" --scale 3 >/dev/null 2>&1

"$server" --data-dir "$data" --port "$port" --mcp >"$data/server.log" 2>&1 &
pid=$!
# A curl probe alone can answer from a server that was already squatting on
# the port while our child dies on the bind — and then every request below
# lands in a stranger's store. Readiness is therefore the child's own
# startup line, which it can only print after the bind succeeded.
ready=""
for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
  if grep -q "listening on" "$data/server.log" 2>/dev/null; then
    ready=yes
    break
  fi
  if ! kill -0 "$pid" 2>/dev/null; then
    break
  fi
  sleep 0.25
done
[ -n "$ready" ] || {
  if grep -qi "address\|bind\|in use" "$data/server.log" 2>/dev/null; then
    echo "another server is already on port $port (set TRAZA_DEMO_PORT)" >&2
  else
    echo "the server did not come up on port $port (set TRAZA_DEMO_PORT):" >&2
    tail -3 "$data/server.log" >&2 2>/dev/null || true
  fi
  exit 1
}

cat <<BANNER

┌──────────────────────────────────────────────────────────────────────┐
│  Traza MCP demo — an agent reading a store it did not write          │
└──────────────────────────────────────────────────────────────────────┘

  server:  http://127.0.0.1:$port/v1/mcp   (started with --mcp)
  client:  curl, one JSON-RPC message per POST

  A real client connects with:
    claude mcp add --transport http traza http://127.0.0.1:$port/v1/mcp
BANNER

# -------------------------------------------------------------- handshake

bold "▸ initialize"
dim "  Version negotiation and capabilities: what a client sends first."
printf '\n'
rpc '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"mcp-demo","version":"1"}}}' |
  $render initialize

bold "▸ tools/list"
dim "  Only what this caller may actually call — a tool it would be refused on is never advertised."
printf '\n'
rpc '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' | $render tools

# --------------------------------------------------------- investigation

call describe_store '{}' \
  "Orientation, and the call to make first: service and model names differ per store, and a guessed one returns an empty result that reads like 'nothing is wrong'."

call top_failures '{"limit":4}' \
  "What is breaking, grouped by signature, each with a trace to open. Shares are of the reported total, not of the rows shown."

call analyze_cost '{"group_by":"model","limit":5}' \
  "Where the tokens and the money went. Counts and sums here are exact."

call list_sessions '{"order_by":"tokens","limit":4}' \
  "Sessions ranked across the whole population, not across a recent page that was then re-sorted."

# One tool's output is the next tool's input: open the trace behind whatever
# the worst signature turned out to be.
trace=$(rpc '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"top_failures","arguments":{"limit":1}}}' | $render trace-id)

if [ -n "$trace" ]; then
  call get_trace "{\"trace_id\":\"$trace\"}" \
    "The trace behind that signature. The shape is usually the answer: repeated siblings are a retry storm, a deep chain is a loop."
fi

call search_spans '{"status":"error","limit":5}' \
  "The general filter. Stored prompts and completions stay out until include_content asks for them."

# ---------------------------------------------------------- the refusals

bold "▸ what the surface refuses"
dim "  The parts that exist to say no, and to say why."
printf '\n'

printf '  a service that does not exist — diagnosed, not answered with silence:\n'
rpc '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"search_spans","arguments":{"service":"api"}}}' |
  $render first-line

printf '\n  a date that is not on the calendar:\n'
rpc '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"search_spans","arguments":{"since":"2026-02-31"}}}' |
  $render whole-text

printf '\n  a REST parameter name a model might reach for:\n'
rpc '{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"search_spans","arguments":{"attr.service":"checkout"}}}' |
  $render whole-text

printf '\n  writing, on a server started without --mcp-annotations:\n'
rpc '{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"record_annotation","arguments":{"trace_id":"t","name":"q","value":1}}}' |
  $render rpc-error

printf '\n  a browser origin the operator did not name (DNS-rebinding defence):\n    '
curl -sS -o /dev/null -w 'HTTP %{http_code}\n' -X POST "http://127.0.0.1:$port/v1/mcp" \
  -H 'Content-Type: application/json' -H 'Origin: https://evil.example.com' \
  -d '{"jsonrpc":"2.0","id":8,"method":"tools/list"}'

printf '\n  and how big a reply actually is, against the 32 KiB default ceiling:\n'
rpc '{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"search_spans","arguments":{"limit":20}}}' |
  $render size

# --------------------------------------------------- resources and prompts

bold "▸ resources/list, resources/templates/list"
dim "  Context a host can attach without a tool call. Templates address one trace, session or payload by id."
printf '\n'
rpc '{"jsonrpc":"2.0","id":10,"method":"resources/list"}' | $render resources
rpc '{"jsonrpc":"2.0","id":11,"method":"resources/templates/list"}' | $render templates

bold "▸ prompts/list"
dim "  Saved investigations. Most hosts surface these as slash commands."
printf '\n'
rpc '{"jsonrpc":"2.0","id":12,"method":"prompts/list"}' | $render prompts

bold "▸ prompts/get debug_failing_session"
dim "  The plan, with this store's live overview attached as a resource."
printf '\n'
rpc '{"jsonrpc":"2.0","id":13,"method":"prompts/get","params":{"name":"debug_failing_session"}}' |
  $render prompt

cat <<'CLOSING'

┌──────────────────────────────────────────────────────────────────────┐
│  Done — the store and its directory are removed on exit.             │
│                                                                      │
│  Every reply above was bounded before it was sent: twenty spans by   │
│  default, stored prompts omitted until asked for, and the whole      │
│  serialized result capped, with any truncation stated. Stored span   │
│  text arrived inside a block marked untrusted.                       │
│                                                                      │
│  Reference: docs/guide/mcp.md                                        │
└──────────────────────────────────────────────────────────────────────┘
CLOSING
