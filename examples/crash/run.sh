#!/bin/sh
# kill -9 mid-ingest, then the receipts.
#
# A client streams spans at a server running --durability wal. The server is
# SIGKILLed while batches are still in flight — no unwinding, no destructors,
# no flush on the way out — then restarted on the same directory, and every
# acknowledged span is counted back out. Then the recovered store is
# hot-backed-up while serving, one byte on disk is flipped, GET /v1/verify
# names the damaged file, and the backup is restored.
#
#   examples/crash/run.sh
#
# It builds what it needs, works in a throwaway directory, and cleans up on
# exit. Every number printed is measured in this run.

set -eu

# multiprocessing re-imports its main module, which would litter __pycache__.
export PYTHONDONTWRITEBYTECODE=1

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
here=$root/examples/crash
cd "$root"

server=./target/release/traza-server
ingest="python3 $here/ingest.py"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/traza-crash-demo.XXXXXX")
port=${TRAZA_DEMO_PORT:-8125}
total=${TRAZA_CRASH_SPANS:-60000}
batch=${TRAZA_CRASH_BATCH:-200}
data=$tmp/data
restored=$tmp/restored
backup=$tmp/backup
pid=""
client=""
watchdog=""

cleanup() {
  [ -n "$watchdog" ] && kill "$watchdog" 2>/dev/null || true
  [ -n "$client" ] && kill "$client" 2>/dev/null || true
  [ -n "$pid" ] && kill -9 "$pid" 2>/dev/null || true
  rm -rf "$tmp"
}
trap cleanup EXIT
# A Ctrl-C is the operator's kill, not the demo's: say so and leave, without
# blaming the contract. exit in a trap still runs the EXIT trap above.
trap 'printf "\ninterrupted\n" >&2; exit 130' INT
trap 'exit 143' TERM

command -v python3 >/dev/null 2>&1 || {
  echo "this demo uses python3 as its ingest client and to read JSON replies" >&2
  exit 1
}
command -v curl >/dev/null 2>&1 || {
  echo "this demo drives the server with curl" >&2
  exit 1
}

bold() { printf '\n\033[1m%s\033[0m\n' "$*"; }
dim() { printf '\033[2m%s\033[0m\n' "$*"; }
fail() { printf '\nFAILED: %s\n' "$*" >&2; exit 1; }

# Start the server with its stdout+stderr captured to $1, wait for readiness.
# Launched through a subshell so this shell never owns it as a job — otherwise
# the shell narrates "Killed: 9" over the output when the SIGKILL beat lands.
start_server() {
  log=$1
  shift
  (
    "$server" --port "$port" --durability wal "$@" >"$log" 2>&1 &
    echo $! >"$tmp/server.pid"
  )
  pid=$(cat "$tmp/server.pid")
  tries=0
  # The curl probe alone can answer from a stranger already squatting on the
  # port while the child dies on the bind — so readiness also requires the
  # child's own startup line, which it prints only after its bind succeeded.
  while :; do
    if grep -q 'listening on' "$log" 2>/dev/null &&
      curl -sS -o /dev/null "http://127.0.0.1:$port/v1/stats" 2>/dev/null; then
      break
    fi
    kill -0 "$pid" 2>/dev/null || {
      if grep -qi 'address\|bind\|in use' "$log" 2>/dev/null; then
        echo "another server is already on port $port (set TRAZA_DEMO_PORT)" >&2
        exit 1
      fi
      sed 's/^/  /' "$log" >&2
      fail "the server exited during startup (log above)"
    }
    tries=$((tries + 1))
    [ "$tries" -lt 40 ] || fail "the server did not come up on port $port (set TRAZA_DEMO_PORT)"
    sleep 0.25
  done
  kill -0 "$pid" 2>/dev/null || {
    echo "another server is already on port $port (set TRAZA_DEMO_PORT)" >&2
    exit 1
  }
}

# ------------------------------------------------------------------ setup

if [ ! -x "$server" ]; then
  echo "building traza-server…"
  cargo build --release --bin traza-server
fi
mkdir -p "$data"

cat <<BANNER

┌──────────────────────────────────────────────────────────────────────┐
│  Traza crash demo — kill -9 mid-ingest, then the receipts            │
└──────────────────────────────────────────────────────────────────────┘

  server:  http://127.0.0.1:$port   (--durability wal)
  client:  ingest.py — counts a span only when a 200 carrying
           {"accepted":N,"durability":"wal"} covers it
  target:  $total spans in batches of $batch, killed midstream
BANNER

# ------------------------------------------------- the promise, in writing

bold "▸ a promise, in writing"
dim "  --durability wal: a 200 means the batch is fsynced to the write-ahead log."
start_server "$tmp/server-1.log" --data-dir "$data"
printf '  %s\n' "$(grep 'durability=wal' "$tmp/server-1.log" | head -1)"

# --------------------------------------------------- the kill, mid-stream

bold "▸ stream $total spans, kill -9 mid-flight"
threshold=$(python3 -c "import random; t=$total; print(t // 2 + random.randint(0, max(1, t // 20)))")
dim "  the kill fires once ~half the target is acknowledged; this run drew $threshold."
mkfifo "$tmp/progress"
# Opened read-write so the open cannot block: a plain read-side open would
# wedge this shell forever if the client died before opening its write end.
exec 3<>"$tmp/progress"
$ingest stream "http://127.0.0.1:$port" "$total" "$batch" "$tmp/progress" "$tmp/client.json" &
client=$!
# Because fd 3 holds the FIFO open, the client's death alone never delivers
# EOF — a watchdog posts a sentinel the moment it exits, however it exits.
(
  while kill -0 "$client" 2>/dev/null; do sleep 0.1; done
  echo client-exited >&3
) &
watchdog=$!
killed=""
kill_at=""
while read -r n <&3; do
  case $n in client-exited) break ;; esac
  if [ -z "$killed" ] && [ "$n" -ge "$threshold" ] 2>/dev/null; then
    kill -9 "$pid"
    killed=yes
    kill_at=$n
  fi
done
exec 3>&-
wait "$watchdog" 2>/dev/null || true
watchdog=""
status=0
wait "$client" || status=$?
client=""
if [ "$status" -ge 128 ]; then
  # 128+n is death by signal — someone killed the client, the demo did not
  # fail. The contract was never put to the question.
  printf '\ninterrupted\n' >&2
  exit "$status"
fi
[ "$status" -eq 0 ] || fail "the ingest client saw a contract violation (its message is above)"
while kill -0 "$pid" 2>/dev/null; do sleep 0.05; done
[ -n "$killed" ] || fail "the client finished before the kill threshold was reached"

acked=$($ingest field "$tmp/client.json" acknowledged)
last_span=$($ingest field "$tmp/client.json" last_span_id)
last_trace=$($ingest field "$tmp/client.json" last_trace_id)
stopped=$($ingest field "$tmp/client.json" stopped)
inflight=$($ingest field "$tmp/client.json" inflight_size)
first_unacked=$($ingest field "$tmp/client.json" first_unacked_seq)

printf '  the kill fired after the client reported %s spans acknowledged; it kept sending and hit:\n' "$kill_at"
printf '    %s\n' "$stopped"
printf '  final tally: %s acknowledged, last acked id %s, %s spans in flight unacknowledged\n' \
  "$acked" "$last_span" "$inflight"
[ "$acked" -lt "$total" ] || fail "the kill did not land mid-stream ($acked of $total acknowledged)"
[ "$acked" -ge "$threshold" ] || fail "acknowledged count $acked is below the kill threshold $threshold"

# ------------------------------------------------------------- the restart

bold "▸ restart on the same directory"
dim "  recovery replays the write-ahead log before the first new write is accepted."
start_server "$tmp/server-2.log" --data-dir "$data"
printf '  %s\n' "$(grep 'durability=wal' "$tmp/server-2.log" | head -1)"
curl -sS "http://127.0.0.1:$port/v1/stats" | $ingest stats

# ------------------------------------------------------------ the receipts

bold "▸ the receipts"
dim "  every crash-test span is streamed back out and counted client-side;"
dim "  the export's own trailer cross-checks the count."
recovered=$(curl -s --raw "http://127.0.0.1:$port/v1/export?service=crash-test" | $ingest export-count)
printf '\n  acknowledged before the kill: %s. present after recovery: %s.\n' "$acked" "$recovered"
[ "$recovered" -ge "$acked" ] || fail "recovered $recovered spans but $acked were acknowledged"

curl -sS "http://127.0.0.1:$port/v1/traces/$last_trace" >"$tmp/trace.json"
$ingest has-span "$tmp/trace.json" "$last_span" ||
  fail "the last acknowledged span $last_span is missing from trace $last_trace"
printf '  the last acknowledged span (%s) is present in trace %s.\n' "$last_span" "$last_trace"

extra=$((recovered - acked))
[ "$extra" -le "$inflight" ] || fail "more spans present ($extra extra) than were ever in flight ($inflight)"
if [ "$inflight" -gt 0 ]; then
  printf '  the in-flight batch (%s spans from seq %s, never acknowledged): %s of %s are present.\n' \
    "$inflight" "$first_unacked" "$extra" "$inflight"
  dim "  no 200 covered that batch, so the contract promises nothing about it either"
  dim "  way — the line above is just what happened this run."
fi

# ------------------------------------------------------------- hot backup

bold "▸ hot backup, while serving"
dim "  POST /v1/backups/crash-demo checkpoints, hard-links the pin, and verifies"
dim "  every digest before reporting success. The server keeps serving throughout."
curl -sS -X POST "http://127.0.0.1:$port/v1/backups/crash-demo" >"$tmp/backup.json"
sed 's/^/  /' "$tmp/backup.json"
printf '\n'
[ "$($ingest field "$tmp/backup.json" verified)" = "true" ] || fail "the backup did not verify"
pin=$($ingest field "$tmp/backup.json" path)
cp -a "$pin" "$backup"
curl -sS -X POST "http://127.0.0.1:$port/v1/backups/crash-demo/release" >"$tmp/release.json"
[ "$($ingest field "$tmp/release.json" released)" = "true" ] || fail "the pin did not release"
dim "  pin copied out with cp -a and released — the copy owns its bytes now."

# -------------------------------------------------------- one flipped byte

bold "▸ one flipped byte"
dim "  a single byte in the middle of the largest segment, corrupted in place,"
dim "  with the server still running."
damage=$($ingest corrupt "$data")
printf '  %s\n' "$damage"
segfile=${damage%% *}
curl -sS "http://127.0.0.1:$port/v1/verify" >"$tmp/verify-damaged.json"
$ingest verify "$tmp/verify-damaged.json" damaged "$segfile" ||
  fail "GET /v1/verify did not name the damaged file $segfile"

# ---------------------------------------------------------------- restore

bold "▸ restore from the backup"
dim "  kill the damaged server, install the copy into a fresh directory, verify,"
dim "  and count the spans again."
kill -9 "$pid" 2>/dev/null || true
while kill -0 "$pid" 2>/dev/null; do sleep 0.05; done
pid=""
mkdir -p "$restored"
start_server "$tmp/server-3.log" --data-dir "$restored" --restore "$backup"
printf '  %s\n' "$(grep 'restored generation' "$tmp/server-3.log" | head -1)"
curl -sS "http://127.0.0.1:$port/v1/verify" >"$tmp/verify-restored.json"
$ingest verify "$tmp/verify-restored.json" intact || fail "the restored store failed verification"
recount=$(curl -s --raw "http://127.0.0.1:$port/v1/export?service=crash-test" | $ingest export-count)
[ "$recount" -eq "$recovered" ] || fail "restored store holds $recount spans, expected $recovered"
printf '\n  present after restore: %s — exactly what the store held when it was pinned.\n' "$recount"

# ---------------------------------------------------------------- closing

cat <<'CLOSING'

┌──────────────────────────────────────────────────────────────────────┐
│  Done — every store this demo created is removed on exit.            │
│                                                                      │
│  What was demonstrated: under --durability wal, a 200 means the      │
│  batch is fsynced to the write-ahead log and is recovered on         │
│  restart. kill -9, a process panic, or an OS crash cannot lose an    │
│  acknowledged span, on any platform. One caveat, stated plainly:     │
│  on macOS a power cut still can, because fsync there does not        │
│  flush the drive's own write cache (F_FULLFSYNC is not used). An     │
│  unacknowledged in-flight batch has no promise in either direction   │
│  — this run printed what actually happened to its own.               │
│                                                                      │
│  Reference: docs/operations/durability.md,                           │
│             docs/operations/backup.md                                │
└──────────────────────────────────────────────────────────────────────┘
CLOSING
