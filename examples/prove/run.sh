#!/bin/sh
# What the store committed, and proof it was not quietly changed.
#
# Spans are ingested and checkpointed; the ledger — a hash chain over every
# published generation, erasure and anchor — is read, verified by the server
# and recomputed independently; an external anchor is recorded; a session is
# erased, and its ledger entry carries a digest, never the session id; an
# evidence pack's digest is recomputed with the Python standard library.
# Then the server is stopped, one byte of a sealed segment is flipped, and
# offline verification names the damaged file and exits 2.
#
#   examples/prove/run.sh
#
# It builds what it needs (or uses TRAZA_BIN), works in a throwaway
# directory, and cleans up on exit. Every number printed is measured in this
# run, and the script exits non-zero if any claim fails.

set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
here=$root/examples/prove

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
tmp=$(mktemp -d "${TMPDIR:-/tmp}/traza-prove.XXXXXX")
data=$tmp/data
# The notary's key and its copy of the signed head live here, outside the
# data directory: an anchor is worth exactly as much as that separation.
notary=$tmp/notary
port=${TRAZA_DEMO_PORT:-8131}
url=http://127.0.0.1:$port
subject=support-2206
pid=""

cleanup() {
  [ -n "$pid" ] && kill "$pid" 2>/dev/null || true
  rm -rf "$tmp"
}
trap cleanup EXIT
trap 'printf "\ninterrupted\n" >&2; exit 130' INT
trap 'exit 143' TERM

command -v python3 >/dev/null 2>&1 || {
  echo "this demo recomputes every digest with python3's standard library" >&2
  exit 1
}
command -v curl >/dev/null 2>&1 || {
  echo "this demo drives the server with curl" >&2
  exit 1
}
# Beat 6 stops the server and verifies its directory offline. A stopped
# server leaves its LOCK behind, and the offline check reclaims it only once
# `kill -0 PID` — the kill executable on PATH, not the shell builtin — says
# the owner is gone. Without one the check refuses the directory as still
# owned, so say so now rather than five beats in.
(
  set -f
  IFS=:
  for dir in $PATH; do [ -f "$dir/kill" ] && [ -x "$dir/kill" ] && exit 0; done
  exit 1
) || {
  echo "this demo's offline check needs a kill executable on PATH: traza-server" >&2
  echo "reclaims a stopped server's directory lock with kill -0 (on Debian and" >&2
  echo "Ubuntu, kill ships in procps)" >&2
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
  curl -sS -o "$2" -w '%{http_code}' -H "Authorization: Bearer $admin" "$url$1"
}
post() {
  curl -sS -o "$3" -w '%{http_code}' -X POST -H "Authorization: Bearer $admin" \
    -H 'Content-Type: application/json' --data-binary "$2" "$url$1"
}
need() { # need GOT WANTED WHAT
  [ "$1" = "$2" ] || fail "$3 — got $1, wanted $2"
}

# ------------------------------------------------------------------ setup

if [ ! -x "$server" ]; then
  echo "building traza-server…"
  cargo build --release --bin traza-server
fi

admin=$(python3 -c 'import secrets; print(secrets.token_hex(16))')
mkdir -p "$data" "$notary"
TRAZA_TOKENS="admin:$admin" "$server" --data-dir "$data" --port "$port" \
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
need "$(get /v1/ledger/head /dev/null)" 200 "the server on port $port did not accept this run's token"

cat <<BANNER

┌──────────────────────────────────────────────────────────────────────┐
│  Traza prove demo — a hash chain, an anchor, one flipped byte        │
└──────────────────────────────────────────────────────────────────────┘

  server:   $url   (one admin token; throwaway store)
  checks:   every head and digest below is recomputed with hashlib from
            the documented formulas (docs/guide/ledger.md) — nothing from
            Traza is used to check Traza
BANNER

# --------------------------------------------------- 1 · ingest, checkpoint

bold "▸ 1 · ingest and checkpoint"
dim "  Three support conversations. Session $subject belongs to a customer who"
dim "  will ask to be forgotten in beat 4."
echo
render ingest "$port" "$admin" "$subject"
code=$(post /v1/checkpoint '{}' "$tmp/checkpoint.json")
need "$code" 200 "checkpointing"
echo "  POST /v1/checkpoint → $(cat "$tmp/checkpoint.json")"

# ---------------------------------------------------------- 2 · the chain

bold "▸ 2 · the chain, and its head"
dim "  One entry per published generation; each head commits to the one before."
echo
code=$(get /v1/ledger/head "$tmp/head.json")
need "$code" 200 "reading the head"
render head "$tmp/head.json"
code=$(get /v1/ledger "$tmp/ledger.json")
need "$code" 200 "reading the chain"
echo
render chain "$tmp/ledger.json"
echo
echo "  GET /v1/ledger/verify — heads, links, the live manifest, every file re-digested:"
code=$(get /v1/ledger/verify "$tmp/verify.json")
need "$code" 200 "verifying the ledger"
render verify-live "$tmp/verify.json"
echo
echo "  and independently, from the file on disk:"
render recompute "$data/ledger.jsonl" "$tmp/head.json" tip

# --------------------------------------------------------- 3 · an anchor

bold "▸ 3 · an anchor"
dim "  The chain has no secret: whoever can write ledger.jsonl can rewrite it and"
dim "  recompute every head. An external countersignature over a head, kept"
dim "  elsewhere, is what makes a later rewrite detectable."
echo
render sign "$tmp/head.json" "$notary/key" "$notary/anchor.json"
cp "$tmp/head.json" "$notary/signed-head.json"
code=$(post /v1/ledger/anchors "$(cat "$notary/anchor.json")" "$tmp/anchor.json")
need "$code" 201 "recording the anchor"
echo "  POST /v1/ledger/anchors →"
render anchored "$tmp/anchor.json"
forged=$(python3 -c 'import json,sys; b=json.load(open(sys.argv[1])); b["target_head"]="0"*64; print(json.dumps(b))' \
  "$notary/anchor.json")
code=$(post /v1/ledger/anchors "$forged" "$tmp/forged.json")
need "$code" 409 "an anchor naming a head the chain never recorded must be refused"
echo "  the same anchor naming a head the chain never had →"
echo "    $code · $(cat "$tmp/forged.json")"

# --------------------------------------------------------- 4 · an erasure

bold "▸ 4 · an erasure, ledgered by digest"
dim "  POST /v1/erasures for session $subject. The chain records that an erasure"
dim "  happened and of what kind — never what was erased."
echo
code=$(post /v1/erasures "{\"subject\": {\"kind\": \"session\", \"session_id\": \"$subject\"}}" "$tmp/erasure.json")
need "$code" 200 "erasing the session"
render erased "$tmp/erasure.json"
code=$(get /v1/ledger "$tmp/ledger2.json")
need "$code" 200 "re-reading the chain"
echo
render chain "$tmp/ledger2.json"
echo
render erasure-entry "$tmp/ledger2.json" "$data/ledger.jsonl" "$subject"
echo
dim "  Anyone who knows the subject can recompute the digest and find the entry;"
dim "  anyone who does not sees 64 hex characters and a salt."

# ------------------------------------------------------- 5 · evidence pack

bold "▸ 5 · an evidence pack, and its digest"
dim "  GET /v1/evidence: the chain, its verification, retention, activity and"
dim "  erasures in one document. Its digest is what a notary would sign."
echo
code=$(get /v1/evidence "$tmp/evidence.json")
need "$code" 200 "fetching the evidence pack"
render evidence "$tmp/evidence.json" "$subject"
code=$(get /v1/ledger/head "$tmp/head-final.json")
need "$code" 200 "reading the final head"

# ------------------------------------------------- 6 · offline, then damage

bold "▸ 6 · offline verification, before and after one flipped byte"
dim "  The server is stopped; traza-server verify --ledger runs read-only"
dim "  against the bare directory."
echo
kill "$pid"
wait "$pid" 2>/dev/null || true
pid=""
rc=0
"$server" verify --ledger --data-dir "$data" >"$tmp/offline-1.txt" 2>&1 || rc=$?
sed 's/^/    /' "$tmp/offline-1.txt"
echo "    exit code: $rc"
need "$rc" 0 "the untouched store should verify intact offline"
echo
damage=$(render flip "$data")
set -- $damage
echo "  one bit flipped at byte $2 of $3 in $1 —"
echo "  a sealed segment the live manifest names:"
echo
rc=0
"$server" verify --ledger --data-dir "$data" >"$tmp/offline-2.txt" 2>&1 || rc=$?
sed 's/^/    /' "$tmp/offline-2.txt"
echo "    exit code: $rc  (0 intact · 2 broken · 1 could not verify)"
need "$rc" 2 "verification of the damaged store should exit 2"
grep -q "$1: digest mismatch" "$tmp/offline-2.txt" ||
  fail "offline verification did not name $1 as damaged"
echo
echo "  the chain itself is untouched — the damage is in the data, and only"
echo "  re-digesting the files finds it:"
render recompute "$data/ledger.jsonl" "$tmp/head-final.json" holds
render check-signature "$data/ledger.jsonl" "$notary/key" "$notary/signed-head.json"

cat <<'CLOSING'

┌──────────────────────────────────────────────────────────────────────┐
│  Done — the store and its directory are removed on exit.             │
│                                                                      │
│  What was shown: every published generation and erasure is a link    │
│  in a hash chain anyone can recompute; verification re-digests the   │
│  live files and names the one that changed; an erasure is ledgered   │
│  by digest, not by name; an evidence pack's digest recomputes with   │
│  the standard library. What it does not show, because it is not      │
│  true: that the chain alone resists a rewrite. It has no secret —    │
│  only an anchor kept outside the store makes history checkable.      │
│                                                                      │
│  Reference: docs/guide/ledger.md                                     │
└──────────────────────────────────────────────────────────────────────┘
CLOSING
