#!/bin/sh
# Traza run fuses as a Claude Code hook.
#
# PreToolUse  (the pre-call gate): asks Traza whether this session's run may
#             take another step. On a `stop` verdict the hook exits 2 with the
#             reasons on stderr, which blocks the tool call and shows the
#             reasons to the model.
# PostToolUse (optional): records the tool call that just ran as a span, so
#             the run's step and repeat limits have something to count. If the
#             acknowledgement says the run is now stopped, exits 2 so the model
#             hears it at once; the tool has already run, so it is the NEXT
#             call that the PreToolUse gate refuses.
#
# Environment:
#   TRAZA_URL                 server base URL (default http://127.0.0.1:8080)
#   TRAZA_TOKEN               bearer token when the server enforces auth: a ro
#                             token is enough for PreToolUse, PostToolUse
#                             needs rw
#   TRAZA_TENANT              the run's tenant, for an unbound token
#   TRAZA_RECORD_TOOL_INPUT   1 records each tool's full input on its span;
#                             by default only a SHA-256 of it is recorded,
#                             which is all repeat detection needs
#   TRAZA_FUSE_FAIL_CLOSED    1 blocks tool calls when Traza cannot answer;
#                             by default the hook fails open
#   TRAZA_TIMEOUT             seconds to wait for Traza (default 5)
#   CLAUDE_SESSION_ID         the run id; defaults to the hook input's
#                             session_id
#
# Requires POSIX sh, curl and python3.

set -u

url="${TRAZA_URL:-http://127.0.0.1:8080}"
input=$(cat)

# Every JSON document is built and read by python3, so no string from the
# hook input is ever spliced into JSON or a command line by the shell.
fields=$(printf '%s' "$input" | python3 -c '
import hashlib, json, os, sys, time, uuid
try:
    hook = json.load(sys.stdin)
except ValueError:
    hook = {}
if not isinstance(hook, dict):
    hook = {}
run = os.environ.get("CLAUDE_SESSION_ID") or hook.get("session_id") or ""
if not isinstance(run, str) or not run:
    sys.exit(3)
event = hook.get("hook_event_name") or "PreToolUse"
tool = hook.get("tool_name") if isinstance(hook.get("tool_name"), str) else ""
tenant = os.environ.get("TRAZA_TENANT") or ""
tool_use_id = hook.get("tool_use_id")
if event == "PostToolUse":
    tool_input = json.dumps(hook.get("tool_input"), sort_keys=True, separators=(",", ":"))
    if os.environ.get("TRAZA_RECORD_TOOL_INPUT") != "1":
        tool_input = "sha256:" + hashlib.sha256(tool_input.encode()).hexdigest()
    now = time.time_ns()
    span = {
        "trace_id": "claude-code-" + hashlib.sha256(run.encode()).hexdigest()[:32],
        "span_id": tool_use_id if isinstance(tool_use_id, str) and tool_use_id else uuid.uuid4().hex[:16],
        "name": tool or "tool",
        "service": "claude-code",
        "start_time_ns": now,
        "end_time_ns": now,
        "attributes": {
            "session.id": run,
            "gen_ai.operation.name": "execute_tool",
            "gen_ai.tool.name": tool or "tool",
            "gen_ai.tool.call.arguments": tool_input,
        },
    }
    if tenant:
        # The span key is the reserved `$tenant`: a bare `tenant` is stored
        # as client data and the span lands in the default tenant, where the
        # gate below (which asks about the named tenant) never sees it.
        span["$tenant"] = tenant
    body = [span]
    path = "/v1/spans"
else:
    body = {"run": run, "next": {"tool": tool} if tool else {}}
    if tenant:
        body["tenant"] = tenant
    path = "/v1/fuses/check"
# One field per line; the run is printed for messages only, made printable.
print(event)
print(path)
print("".join(c if c.isprintable() else "?" for c in run))
print(json.dumps(body))
')
case $? in
0) ;;
3) echo "traza fuse: no session id in the hook input; not checking" >&2; exit 0 ;;
*) echo "traza fuse: could not read the hook input" >&2; exit 0 ;;
esac

event=$(printf '%s\n' "$fields" | sed -n 1p)
path=$(printf '%s\n' "$fields" | sed -n 2p)
run=$(printf '%s\n' "$fields" | sed -n 3p)
body=$(printf '%s\n' "$fields" | sed -n 4p)

unanswered() {
    if [ "${TRAZA_FUSE_FAIL_CLOSED:-0}" = 1 ]; then
        echo "traza fuse: $1, and TRAZA_FUSE_FAIL_CLOSED=1 blocks the call" >&2
        exit 2
    fi
    echo "traza fuse: $1; letting the call through" >&2
    exit 0
}

# The token travels in a curl config read from a here-document on fd 3, so it
# never appears on a command line another process could read.
auth_config() {
    if [ -n "${TRAZA_TOKEN:-}" ]; then
        printf 'header = "Authorization: Bearer %s"\n' "$TRAZA_TOKEN"
    fi
}
response=$(printf '%s' "$body" | curl -sS --max-time "${TRAZA_TIMEOUT:-5}" \
    -X POST -H 'Content-Type: application/json' --data-binary @- \
    -w '\n%{http_code}' -K /dev/fd/3 "$url$path" 3<<EOF
$(auth_config)
EOF
) || unanswered "Traza did not answer at $url"

status=$(printf '%s\n' "$response" | tail -n 1)
payload=$(printf '%s\n' "$response" | sed '$d')
[ "$status" = 200 ] || unanswered "Traza answered $path with HTTP $status"

# The verdict on the first line, then one reason per line.
verdict=$(printf '%s' "$payload" | python3 -c '
import json, sys
doc = json.load(sys.stdin)
if "verdict" in doc:
    verdict, reasons = doc["verdict"], doc.get("reasons", [])
else:
    # An ingest acknowledgement of our one span: `fuses` names its run only
    # when the run is not `continue`.
    fuses = doc.get("fuses") or [{}]
    verdict, reasons = fuses[0].get("verdict", "continue"), fuses[0].get("reasons", [])
print(verdict)
for reason in reasons:
    print(reason)
') || unanswered "Traza answered with something that is not a verdict"

case $(printf '%s\n' "$verdict" | sed -n 1p) in
stop)
    {
        echo "Traza stopped run $run. Do not continue this task; report why to the user."
        printf '%s\n' "$verdict" | sed '1d' | sed 's/^/  - /'
        [ "$event" = PostToolUse ] && echo "  (This tool call already ran; the next one will be refused.)"
    } >&2
    exit 2
    ;;
warn)
    {
        echo "Traza warns run $run:"
        printf '%s\n' "$verdict" | sed '1d' | sed 's/^/  - /'
    } >&2
    ;;
esac
exit 0
