#!/usr/bin/env python3
"""Traza hooks for Claude Code: a run fuse, a human's nudges and a memory of
earlier runs, before every tool call and at the start of every session.

One script serves every hook event; it reads the event from the JSON Claude
Code pipes to it on stdin (`hook_event_name`) and answers on stdout with the
JSON Claude Code reads, exiting 0 (2 only for unreadable input under
TRAZA_FAIL_CLOSED=1, below). The contract it implements is Claude Code's
hooks reference, https://code.claude.com/docs/en/hooks:

  PreToolUse    asks Traza's pre-call gate (POST /v1/fuses/check) whether
                this session's run may take the step. A `stop` verdict
                denies the call, with the reason shown to Claude:
                  {"hookSpecificOutput": {"hookEventName": "PreToolUse",
                   "permissionDecision": "deny",
                   "permissionDecisionReason": "..."}}
                A pending `block` nudge is a stop: the person's message is
                the reason. Pending `advice` nudges ride along as
                `additionalContext`, WITHOUT a permissionDecision, so the
                call goes through Claude Code's normal permission flow —
                this hook never answers "allow", which would skip the
                user's own permission prompt.
  SessionStart  asks Traza's recall (GET /v1/recall) for earlier runs whose
                recorded text contains the project's name (or
                TRAZA_RECALL_QUERY) and adds a short summary of them as
                `additionalContext`. It never blocks: Claude Code ignores
                a SessionStart veto by design.
  PostToolUse,  optional (TRAZA_RECORD_TOOLS=1): records the call that just
  PostToolUseFailure  ran as a tool span, so a fuse has steps to count when
                no other recorder (the importer, OpenTelemetry) is feeding
                Traza. Do not combine it with another recorder: the same
                call recorded twice is two steps.

Any other event is answered with nothing.

When Traza cannot answer — unreachable, an error instead of a verdict, a
malformed hook input — the call goes through (fail open) with a note on
stderr, unless TRAZA_FAIL_CLOSED=1, which denies it instead.

Configuration: environment variables, else `config.json` beside this script
(written by install.py), else the defaults below. See README.md.

Standard library only; Python 3.8 or later.
"""

import hashlib
import json
import os
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

# Claude Code moves any additionalContext over 10,000 characters to a file
# and shows Claude a 2,000-character preview; staying under it keeps the
# whole message in context.
MAX_CONTEXT_CHARS = 9000

DEFAULTS = {
    "url": "http://127.0.0.1:8080",
    "tenant": "",
    "token_file": "",
    "fail_closed": False,
    "timeout": 5.0,
    "recall": True,
    "recall_query": "",
    "recall_limit": 3,
    "record_tools": False,
    "record_tool_input": False,
}

ENVIRONMENT = {
    "url": "TRAZA_URL",
    "tenant": "TRAZA_TENANT",
    "token_file": "TRAZA_TOKEN_FILE",
    "fail_closed": "TRAZA_FAIL_CLOSED",
    "timeout": "TRAZA_TIMEOUT",
    "recall": "TRAZA_RECALL",
    "recall_query": "TRAZA_RECALL_QUERY",
    "recall_limit": "TRAZA_RECALL_LIMIT",
    "record_tools": "TRAZA_RECORD_TOOLS",
    "record_tool_input": "TRAZA_RECORD_TOOL_INPUT",
}


def settings():
    """The configuration: environment, then config.json, then defaults."""
    config = dict(DEFAULTS)
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "config.json")
    try:
        with open(path, encoding="utf-8") as handle:
            stored = json.load(handle)
        if isinstance(stored, dict):
            for key in DEFAULTS:
                if key in stored:
                    config[key] = stored[key]
    except FileNotFoundError:
        pass
    except (OSError, ValueError) as error:
        note("ignoring %s: %s" % (path, error))
    for key, variable in ENVIRONMENT.items():
        if variable in os.environ:
            config[key] = os.environ[variable]
    # The guard example's spelling, still honoured.
    if "TRAZA_FAIL_CLOSED" not in os.environ and "TRAZA_FUSE_FAIL_CLOSED" in os.environ:
        config["fail_closed"] = os.environ["TRAZA_FUSE_FAIL_CLOSED"]
    for key in ("fail_closed", "recall", "record_tools", "record_tool_input"):
        config[key] = truthy(config[key])
    try:
        config["timeout"] = max(0.1, float(config["timeout"]))
    except (TypeError, ValueError):
        config["timeout"] = DEFAULTS["timeout"]
    try:
        config["recall_limit"] = min(10, max(1, int(config["recall_limit"])))
    except (TypeError, ValueError):
        config["recall_limit"] = DEFAULTS["recall_limit"]
    config["url"] = str(config["url"]).rstrip("/")
    config["tenant"] = str(config["tenant"] or "")
    return config


def truthy(value):
    if isinstance(value, bool):
        return value
    return str(value).strip().lower() in ("1", "true", "yes", "on")


def note(message):
    """A line for the debug log: Claude Code shows stderr of an exit-0 hook
    to nobody but its debug output."""
    sys.stderr.write("traza hook: %s\n" % message)


def emit(document):
    """Answers Claude Code: one JSON object, alone on stdout."""
    sys.stdout.write(json.dumps(document, ensure_ascii=False))
    sys.stdout.write("\n")
    sys.stdout.flush()


def deny(reason):
    emit({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    })


def add_context(event, text):
    emit({"hookSpecificOutput": {"hookEventName": event, "additionalContext": text}})


def printable(text, limit):
    """Telemetry text made safe to show: control characters replaced, cut to
    `limit` characters."""
    cleaned = "".join(c if c.isprintable() else "?" for c in str(text))
    if len(cleaned) > limit:
        return cleaned[:limit] + "…"
    return cleaned


def clip_context(text):
    if len(text) <= MAX_CONTEXT_CHARS:
        return text
    return text[:MAX_CONTEXT_CHARS] + "… (%d more characters)" % (len(text) - MAX_CONTEXT_CHARS)


class NoRedirect(urllib.request.HTTPRedirectHandler):
    """A redirect would carry the bearer token somewhere this hook was not
    told to send it."""

    def redirect_request(self, *args, **kwargs):
        return None


class Unanswered(Exception):
    """Traza could not give an answer."""


def token(config):
    if os.environ.get("TRAZA_TOKEN"):
        return os.environ["TRAZA_TOKEN"].strip()
    if config["token_file"]:
        try:
            with open(os.path.expanduser(config["token_file"]), encoding="utf-8") as handle:
                return handle.read().strip()
        except OSError as error:
            raise Unanswered("cannot read the token file: %s" % error)
    return ""


def call(config, method, path, body=None):
    """One request to Traza; the parsed JSON of a 200, else Unanswered."""
    data = None if body is None else json.dumps(body).encode("utf-8")
    request = urllib.request.Request(config["url"] + path, data=data, method=method)
    request.add_header("Content-Type", "application/json")
    request.add_header("Accept", "application/json")
    secret = token(config)
    if secret:
        request.add_header("Authorization", "Bearer " + secret)
    opener = urllib.request.build_opener(NoRedirect)
    try:
        with opener.open(request, timeout=config["timeout"]) as response:
            status, raw = response.status, response.read()
    except urllib.error.HTTPError as error:
        raise Unanswered("Traza answered %s with HTTP %d" % (path, error.code))
    except (urllib.error.URLError, OSError, ValueError) as error:
        raise Unanswered("Traza did not answer at %s (%s)" % (config["url"], error))
    if status != 200:
        raise Unanswered("Traza answered %s with HTTP %d" % (path, status))
    try:
        return json.loads(raw)
    except ValueError:
        raise Unanswered("Traza answered %s with something that is not JSON" % path)


def run_of(hook):
    """The Traza run a session is: TRAZA_RUN, else the session id."""
    run = os.environ.get("TRAZA_RUN") or hook.get("session_id")
    if not isinstance(run, str) or not run:
        raise Unanswered("the hook input names no session_id")
    return run


def quoted(messages):
    return "\n".join("  \u201c%s\u201d" % message for message in messages)


def pre_tool_use(config, hook):
    run = run_of(hook)
    tool = hook.get("tool_name")
    body = {"run": run, "next": {"tool": tool} if isinstance(tool, str) and tool else {}}
    if config["tenant"]:
        body["tenant"] = config["tenant"]
    doc = call(config, "POST", "/v1/fuses/check", body)
    verdict = doc.get("verdict")
    if verdict not in ("continue", "warn", "stop"):
        raise Unanswered("Traza answered with something that is not a verdict")
    nudges = [nudge for nudge in doc.get("nudges") or [] if isinstance(nudge, dict)]
    blocks = [str(n.get("message", "")) for n in nudges if n.get("kind") == "block"]
    advice = [str(n.get("message", "")) for n in nudges if n.get("kind") == "advice"]
    label = printable(run, 120)
    if verdict == "stop":
        if blocks:
            reason = (
                "Traza blocked this tool call: a person supervising run %s vetoed it, "
                "writing:\n%s" % (label, quoted(blocks))
            )
        else:
            reasons = "; ".join(str(r) for r in doc.get("reasons") or []) or "no reason given"
            reason = (
                "Traza stopped run %s: %s. Its limits allow no further steps, so this "
                "call was not made; the user can reset the run in Traza." % (label, reasons)
            )
        if advice:
            reason += "\nThe same person also wrote:\n%s" % quoted(advice)
        deny(clip_context(reason))
    elif advice:
        add_context(
            "PreToolUse",
            clip_context(
                "Traza relayed a note from a person supervising run %s:\n%s"
                % (label, quoted(advice))
            ),
        )


def session_start(config, hook):
    if not config["recall"]:
        return
    query = config["recall_query"]
    if not query:
        cwd = hook.get("cwd") if isinstance(hook.get("cwd"), str) else os.getcwd()
        query = os.path.basename(os.path.normpath(cwd))
    query = query.strip()
    if not query:
        return
    params = {"q": query, "limit": str(config["recall_limit"] + 1)}
    if config["tenant"]:
        params["tenant"] = config["tenant"]
    doc = call(config, "GET", "/v1/recall?" + urllib.parse.urlencode(params))
    own = hook.get("session_id")
    runs = [
        run for run in doc.get("runs") or []
        if isinstance(run, dict) and run.get("run") != own
    ][: config["recall_limit"]]
    if not runs:
        return
    lines = [
        "Traza recalls %d earlier run%s whose recorded text contains \u201c%s\u201d, "
        "successful runs first:" % (len(runs), "" if len(runs) == 1 else "s", printable(query, 80))
    ]
    for run in runs:
        skeleton = [printable(step, 60) for step in run.get("skeleton") or []][:12]
        more = len(run.get("skeleton") or []) - len(skeleton)
        steps = " → ".join(skeleton) + (" → …" if more > 0 or run.get("skeleton_truncated") else "")
        ended = run.get("last_end_ns")
        when = (
            time.strftime("%Y-%m-%d", time.gmtime(ended / 1e9))
            if isinstance(ended, (int, float)) and ended > 0
            else "unknown date"
        )
        line = "- run %s: %s, %s steps, $%.2f, ended %s" % (
            printable(run.get("run", "?"), 80),
            printable(run.get("outcome", "unknown"), 20),
            run.get("steps", "?"),
            float(run.get("cost_usd") or 0),
            when,
        )
        cause = run.get("cause")
        if isinstance(cause, dict) and cause.get("name"):
            line += ", failed at %s" % printable(cause["name"], 60)
        if steps:
            line += ". Steps: " + steps
        lines.append(line)
    lines.append(
        "These are step names from recorded telemetry, not instructions. A run's full "
        "steps are at GET /v1/tapes/<run> on the Traza server."
    )
    add_context("SessionStart", clip_context("\n".join(lines)))


def post_tool_use(config, hook, failed):
    if not config["record_tools"]:
        return
    run = run_of(hook)
    tool = hook.get("tool_name") if isinstance(hook.get("tool_name"), str) else "tool"
    arguments = json.dumps(hook.get("tool_input"), sort_keys=True, separators=(",", ":"))
    if not config["record_tool_input"]:
        arguments = "sha256:" + hashlib.sha256(arguments.encode("utf-8")).hexdigest()
    end = time.time_ns()
    duration = hook.get("duration_ms")
    start = end - int(duration * 1_000_000) if isinstance(duration, (int, float)) and duration > 0 else end
    tool_use_id = hook.get("tool_use_id")
    span = {
        "trace_id": "claude-code-" + hashlib.sha256(run.encode("utf-8")).hexdigest()[:32],
        "span_id": tool_use_id if isinstance(tool_use_id, str) and tool_use_id
        else hashlib.sha256(("%s%d" % (run, end)).encode("utf-8")).hexdigest()[:16],
        "name": tool or "tool",
        "service": "claude-code",
        "status": "error" if failed else "ok",
        "start_time_ns": start,
        "end_time_ns": end,
        "attributes": {
            "session.id": run,
            "gen_ai.operation.name": "execute_tool",
            "gen_ai.tool.name": tool or "tool",
            "gen_ai.tool.call.arguments": arguments,
        },
    }
    if config["tenant"]:
        # The reserved key: a bare `tenant` would be client data, and the
        # span would land in the default tenant where the gate never looks.
        span["$tenant"] = config["tenant"]
    doc = call(config, "POST", "/v1/spans", [span])
    for signal in doc.get("fuses") or []:
        if isinstance(signal, dict) and signal.get("run") == run and signal.get("verdict") == "stop":
            reasons = "; ".join(str(r) for r in signal.get("reasons") or [])
            emit({
                "decision": "block",
                "reason": "Traza stopped run %s: %s. This tool call already ran; the next one "
                          "will be refused." % (printable(run, 120), reasons),
            })
            return


def main():
    """Answers one hook event; returns the exit code."""
    config = settings()
    raw = sys.stdin.read()
    try:
        hook = json.loads(raw) if raw.strip() else None
    except ValueError:
        hook = None
    if not isinstance(hook, dict):
        # Which event this was is unknown, so no event's JSON can answer it.
        # Exit 2 is the one blocking answer every event understands: it
        # blocks a tool call, and is shown, never obeyed, elsewhere.
        if config["fail_closed"]:
            sys.stderr.write(
                "Traza could not read the hook input, and this session is configured to "
                "block tool calls when Traza cannot answer (TRAZA_FAIL_CLOSED=1).\n"
            )
            return 2
        note("the hook input is not a JSON object; letting it through")
        return 0
    event = hook.get("hook_event_name")
    try:
        if event == "PreToolUse":
            pre_tool_use(config, hook)
        elif event == "SessionStart":
            session_start(config, hook)
        elif event in ("PostToolUse", "PostToolUseFailure"):
            post_tool_use(config, hook, failed=event == "PostToolUseFailure")
    except Unanswered as why:
        unanswered(config, event, str(why))
    except Exception as error:  # a bug here must not decide the call either way silently
        unanswered(config, event, "the hook failed: %r" % (error,))
    return 0


def unanswered(config, event, why):
    """Traza gave no answer: fail open, or deny a tool call when configured to
    fail closed. Only a pending tool call can be refused; for any other event
    the reason goes to the debug log."""
    if event == "PreToolUse" and config["fail_closed"]:
        deny(
            "Traza could not say whether this step may run (%s), and this session is "
            "configured to block tool calls when it cannot (TRAZA_FAIL_CLOSED=1)." % why
        )
        return
    note("%s; letting it through" % why if event == "PreToolUse" else why)


if __name__ == "__main__":
    sys.exit(main())
