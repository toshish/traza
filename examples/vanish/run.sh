#!/bin/sh
# Deletion with a receipt.
#
# Two tenants share one store and even share trace ids. One tenant's user
# demands deletion. The erasure is tenant-precise and replay-proof, and the
# receipt re-checks every place the bytes could be — catching the backup pin
# that still holds them.
#
#   examples/vanish/run.sh
#
# It builds what it needs, seeds a throwaway store, and cleans up on exit.
# Every number printed below is measured against the live server.

set -eu

# multiprocessing re-imports its main module, which would litter __pycache__.
export PYTHONDONTWRITEBYTECODE=1

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
here=$root/examples/vanish
cd "$root"

server=./target/release/traza-server
seed=./target/release/seed
render="python3 $here/render.py"
data=$(mktemp -d "${TMPDIR:-/tmp}/traza-vanish.XXXXXX")
port=${TRAZA_DEMO_PORT:-8128}
scale=${TRAZA_VANISH_SCALE:-2}
pid=""

cleanup() {
  [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  rm -rf "$data"
}
trap cleanup EXIT INT TERM

command -v python3 >/dev/null 2>&1 || {
  echo "this demo uses python3 to generate tokens, render replies, and race the erasure" >&2
  exit 1
}
command -v curl >/dev/null 2>&1 || {
  echo "this demo drives the server with curl" >&2
  exit 1
}

bold() { printf '\n\033[1m%s\033[0m\n' "$*"; }
dim() { printf '\033[2m%s\033[0m\n' "$*"; }

# HTTP helpers: body lands in a file, the status code is the return value.
get() { curl -sS -o "$3" -w '%{http_code}' -H "Authorization: Bearer $1" "http://127.0.0.1:$port$2"; }
post() { curl -sS -o "$3" -w '%{http_code}' -X POST -H "Authorization: Bearer $1" "http://127.0.0.1:$port$2"; }

need() { # need GOT WANTED WHAT
  [ "$1" = "$2" ] || {
    printf 'ASSERTION FAILED: %s — got %s, wanted %s\n' "$3" "$1" "$2" >&2
    exit 1
  }
}

# ------------------------------------------------------------------ setup

if [ ! -x "$server" ] || [ ! -x "$seed" ]; then
  echo "building traza-server and seed…"
  cargo build --release --bin traza-server --bin seed
fi

admin_tok=$(python3 -c 'import secrets; print(secrets.token_hex(16))')
acme_tok=$(python3 -c 'import secrets; print(secrets.token_hex(16))')
zenith_tok=$(python3 -c 'import secrets; print(secrets.token_hex(16))')

TRAZA_TOKENS="admin:$admin_tok,rw@acme:$acme_tok,rw@zenith:$zenith_tok" \
  "$server" --data-dir "$data" --port "$port" >"$data/server.log" 2>&1 &
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
# The probe answering proves a server is up — not that it is ours: a squatter
# answers the poll while our child is still starting toward its failed bind.
# So demand the port holder's identity papers: it must accept the admin token
# minted for this run (a stranger cannot know it) and refuse the bare request
# (our child always runs with TRAZA_TOKENS). And the child must be alive —
# a dead one means something else owns the port, and every request below
# would land in a stranger's store.
kill -0 "$pid" 2>/dev/null || {
  echo "another server is already on port $port (set TRAZA_DEMO_PORT)" >&2
  exit 1
}
with_tok=$(get "$admin_tok" "/v1/stats" /dev/null) || with_tok=000
without_tok=$(curl -sS -o /dev/null -w '%{http_code}' "http://127.0.0.1:$port/v1/stats") || without_tok=000
[ "$with_tok" = 200 ] && [ "$without_tok" = 401 ] || {
  echo "another server is already on port $port (set TRAZA_DEMO_PORT)" >&2
  exit 1
}

cat <<BANNER

┌──────────────────────────────────────────────────────────────────────┐
│  Traza vanish demo — deletion with a receipt                         │
└──────────────────────────────────────────────────────────────────────┘

  server:   http://127.0.0.1:$port   (throwaway store in $data)
  tokens:   admin (unbound) · rw@acme · rw@zenith
  tenants:  acme and zenith, seeded with IDENTICAL corpora — the same
            seed value, so the same trace ids, span ids, and session ids
            land in both. The tenant lives in the primary key
            (tenant, trace_id, span_id); nothing else tells them apart.
BANNER

echo
echo "seeding both tenants over the live API (bound tokens stamp the tenant):"
out=$(TRAZA_TOKEN=$acme_tok "$seed" --url "http://127.0.0.1:$port" --scale "$scale" --seed 7 2>&1 | tail -1)
echo "  acme:   ${out#seed: }"
out=$(TRAZA_TOKEN=$zenith_tok "$seed" --url "http://127.0.0.1:$port" --scale "$scale" --seed 7 2>&1 | tail -1)
echo "  zenith: ${out#seed: }"

# The session that will be erased: the widest-spread acme conversation.
code=$(get "$acme_tok" "/v1/sessions?limit=200" "$data/sessions.json")
need "$code" 200 "listing acme sessions"
picked=$($render pick-session "$data/sessions.json")
set -- $picked
sid=$1

echo
echo "the subject-to-be: acme session '$sid' — plus a support annotation"
echo "and an exported transcript, so the erasure has every domain to reach:"
$render enrich "$port" "$acme_tok" "$zenith_tok" "$sid" "$data/shared_ref" "$data/blob_bytes"
shared_ref=$(cat "$data/shared_ref")
blob_bytes=$(cat "$data/blob_bytes")

# --------------------------------------------------- 1 · identity in the key

bold "▸ 1 · identity in the key"
dim "  The same trace id exists in both tenants. Each bound token reads its own copy;"
dim "  the fences below are printed verbatim, straight off the wire."

code=$(get "$acme_tok" "/v1/spans?limit=1" "$data/first.json")
need "$code" 200 "reading acme's first span"
shared_trace=$($render pick-trace "$data/first.json")
echo
echo "  shared trace id: $shared_trace"
code=$(get "$acme_tok" "/v1/traces/$shared_trace" "$data/trace_acme.json")
need "$code" 200 "acme reading its copy"
code=$(get "$zenith_tok" "/v1/traces/$shared_trace" "$data/trace_zenith.json")
need "$code" 200 "zenith reading its copy"
$render trace-copies "$data/trace_acme.json" "$data/trace_zenith.json"

echo
echo "  acme token naming zenith in a query — refused, and it says why:"
code=$(get "$acme_tok" "/v1/traces/$shared_trace?tenant=zenith" "$data/fence1.json")
printf '    %s  [%s]\n' "$(cat "$data/fence1.json")" "$code"
need "$code" 403 "naming a foreign tenant"

echo
echo "  a trace only zenith wrote, read with the acme token — 404, never 403,"
echo "  because existence is a fact about another tenant:"
now_ns=$(python3 -c 'import time; print(time.time_ns())')
code=$(curl -sS -o "$data/ing.json" -w '%{http_code}' -X POST \
  -H "Authorization: Bearer $zenith_tok" -H 'Content-Type: application/json' \
  -d "[{\"trace_id\":\"trace-zenith-private-0001\",\"span_id\":\"span-zp-01\",\"name\":\"billing.review\",\"service\":\"support\",\"status\":\"ok\",\"start_time_ns\":$now_ns,\"end_time_ns\":$now_ns,\"attributes\":{}}]" \
  "http://127.0.0.1:$port/v1/spans")
need "$code" 200 "zenith writing its private trace"
code=$(get "$acme_tok" "/v1/traces/trace-zenith-private-0001" "$data/fence2.json")
printf '    %s  [%s]\n' "$(cat "$data/fence2.json")" "$code"
need "$code" 404 "cross-tenant read must be 404"

echo
echo "  acme token on the store-global operator surface — even volumes disclose"
echo "  co-tenants, so bound tokens are refused whole:"
code=$(get "$acme_tok" "/v1/stats" "$data/fence3.json")
printf '    GET /v1/stats → %s  [%s]\n' "$(cat "$data/fence3.json")" "$code"
need "$code" 403 "bound token on /v1/stats"

# ----------------------------------------------------------- 2 · the ledger

bold "▸ 2 · the ledger"
dim "  GET /v1/tenants, admin token: what each tenant holds right now — an exact"
dim "  fold over a pinned snapshot, not a cached estimate."
echo
code=$(get "$admin_tok" "/v1/tenants" "$data/tenants.json")
need "$code" 200 "tenant accounting"
$render tenants "$data/tenants.json" "$blob_bytes"

# -------------------------------------------------------------- 3 · the pin

bold "▸ 3 · the pin"
dim "  A backup is taken the way operations would: pin, verify, copy. The pin"
dim "  hard-links the live generation — and will still hold these bytes after"
dim "  the erasure. That is the point of a pin, and beat 7 will catch it."
echo
code=$(post "$admin_tok" "/v1/backups/pre-erasure" "$data/pin.json")
need "$code" 201 "pinning the pre-erasure backup"
$render pin "$data/pin.json"

# ---------------------------------------------------------- 4 · the request

bold "▸ 4 · the request"
dim "  The user behind session '$sid' demands deletion. POST /v1/erasures,"
dim "  admin token, subject {kind: session, session_id, tenant: acme}. The 200"
dim "  returns only after the purge settles — and while it is pending, one"
dim "  covered span is re-POSTed concurrently (that is beat 6's evidence)."
echo
code=$(get "$acme_tok" "/v1/sessions/$sid" "$data/pre.json")
need "$code" 200 "reading the session before erasure"
pre_count=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['span_count'])" "$data/pre.json")
echo "  before: acme session '$sid' holds $pre_count spans"
echo
code=$(get "$admin_tok" "/v1/metrics" "$data/metrics_before.txt")
need "$code" 200 "metrics before"
$render erase-race "$port" "$admin_tok" "$acme_tok" "$sid" "$pre_count" \
  "$shared_ref" "$data/stash.json" "$data/vanish.env"
. "$data/vanish.env"
code=$(get "$admin_tok" "/v1/metrics" "$data/metrics_after.txt")
need "$code" 200 "metrics after"

# ------------------------------------------------------ 5 · tenant precision

bold "▸ 5 · tenant precision"
dim "  The SAME session id, the SAME trace ids — zenith's copy is untouched,"
dim "  acme's is gone. Deletion cannot bleed across the primary key."
echo
code=$(get "$zenith_tok" "/v1/sessions/$sid" "$data/zenith_session.json")
need "$code" 200 "zenith reading the shared session id"
$render session-assert "$data/zenith_session.json" "$pre_count"
code=$(get "$zenith_tok" "/v1/traces/$covered_trace" "$data/zenith_trace.json")
need "$code" 200 "zenith reading the covered trace id"
echo "  zenith's copy of covered trace $covered_trace: $($render trace-count "$data/zenith_trace.json") span(s), still readable [200]"
echo
code=$(get "$acme_tok" "/v1/sessions/$sid" "$data/acme_session.json")
printf '  acme'\''s same session id: %s  [%s]\n' "$(cat "$data/acme_session.json")" "$code"
need "$code" 404 "acme's session must be gone"
code=$(get "$acme_tok" "/v1/spans?session=$sid" "$data/acme_spans.json")
need "$code" 200 "acme searching the erased session"
$render gone "$data/acme_spans.json"

# --------------------------------------------------------- 6 · replay-proof

bold "▸ 6 · replay-proof"
dim "  A client with a retry queue replays a covered span while the erasure is"
dim "  pending: acknowledged, deliberately not stored, and counted. The response"
dim "  below was captured during beat 4's pending window."
echo
$render replay "$data/stash.json" "$data/metrics_before.txt" "$data/metrics_after.txt" \
  traza_erasure_spans_suppressed_total
echo
code=$(get "$acme_tok" "/v1/traces/$covered_trace" "$data/replay_read.json")
printf '  and the replayed span is not readable: %s  [%s]\n' "$(cat "$data/replay_read.json")" "$code"
need "$code" 404 "the replayed span must not be readable"

# ---------------------------------------------------------- 7 · the receipt

bold "▸ 7 · the receipt"
dim "  GET /v1/erasures/$eid/verify re-checks every domain the bytes could"
dim "  inhabit — computed from what the walk finds, never from what the settle"
dim "  record claims."
echo
code=$(get "$admin_tok" "/v1/erasures/$eid/verify" "$data/receipt1.json")
need "$code" 200 "first verification"
$render receipt "$data/receipt1.json" pinned pre-erasure
echo
echo "  The receipt refused to say \"gone\": the pre-erasure pin still holds the"
echo "  bytes in its hard-link farm, and the receipt names it instead of hiding"
echo "  it. Release the pin, verify again:"
echo
code=$(post "$admin_tok" "/v1/backups/pre-erasure/release" "$data/release.json")
need "$code" 200 "releasing the pin"
$render release "$data/release.json"
code=$(get "$admin_tok" "/v1/erasures/$eid/verify" "$data/receipt2.json")
need "$code" 200 "second verification"
$render receipt "$data/receipt2.json" final pre-erasure

# ------------------------------------------------------------- 8 · epilogue

bold "▸ 8 · epilogue — the receipt without the server"
dim "  Stop the server; the same verification runs offline, against the bare"
dim "  data directory. Exit 0 means erased and conclusive."
echo
kill "$pid"
wait "$pid" 2>/dev/null || true
pid=""
rc=0
"$server" verify --erasure "$eid" --data-dir "$data" >"$data/offline.txt" 2>&1 || rc=$?
sed 's/^/  /' "$data/offline.txt"
echo
echo "  exit code: $rc  (0 = erased and conclusive; 3 = erased but inconclusive; 2 = did not hold)"
need "$rc" 0 "offline receipt must be erased and conclusive"

cat <<'CLOSING'

┌──────────────────────────────────────────────────────────────────────┐
│  Done — the store and its directory are removed on exit.             │
│                                                                      │
│  What was proved: two tenants can share ids and bytes, and one       │
│  tenant's erasure removes exactly its own — spans, annotations,      │
│  payload references — while a replay during the purge is absorbed,   │
│  a shared payload is retained by name, and the receipt refuses to    │
│  claim proof while a pinned backup still holds the data.             │
│                                                                      │
│  Reference: docs/guide/http-api.md#erasure                           │
│             docs/operations/administration.md                        │
└──────────────────────────────────────────────────────────────────────┘
CLOSING
