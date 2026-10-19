#!/usr/bin/env python3
"""The foresight demo's agent: a scripted support agent with a careful harness.

The *model* and the *tools* are scripted — one script per branch of the
behaviour corpus — so the demo needs no provider and no key. The *harness*
is what a real one would be under Traza:

  1. before each step, ask POST /v1/fuses/check whether the run may take it,
     naming the tool when the step is a tool call, and halt on `stop`;
  2. after each step, export its span (POST /v1/spans), the way an
     OpenTelemetry GenAI exporter would;
  3. and, for the demo's sake, ask GET /v1/fate/runs/{run} what runs that
     took the same path went on to do.

State lives in a JSON file, so the demo can stop the agent, add a policy
from outside, and start it again where it left off. Exit codes: 0 finished,
3 halted by Traza, 4 paused at --max-steps, 1 anything else — a redirect
included: one is never followed, so the token goes only to the URL the agent
was given.

  agent.py run URL TOKEN RUN BRANCH STATE OUT [MAX_STEPS]
"""

import hashlib
import json
import os
import sys
import time
import urllib.error
import urllib.request

SERVICE = "support-agent"
MODEL = "gpt-6.1-sol"

# The branches, as the corpus's support-agent takes them (src/seed.rs):
# (kind, name, status). An empty search is recorded as an error, as the
# corpus records it.
BRANCHES = {
    # The search comes back empty, and the agent runs it again — and again.
    "doomed": [("llm", MODEL, "ok"), ("tool", "search_orders", "error"),
               ("tool", "search_orders", "error"), ("tool", "search_orders", "error"),
               ("tool", "search_orders", "error"), ("llm", MODEL, "error")],
    # The order is found, refunded once, and the customer told.
    "healthy": [("llm", MODEL, "ok"), ("tool", "search_orders", "ok"), ("llm", MODEL, "ok"),
                ("tool", "issue_refund", "ok"), ("llm", MODEL, "ok")],
}
SECOND = 1_000_000_000


class RedirectRefused(Exception):
    """A 3xx answered a request that carried the token."""


class RefuseRedirects(urllib.request.HTTPRedirectHandler):
    """Refuses every redirect instead of following it.

    urllib copies a request's headers onto the redirected one, Authorization
    included. Traza never redirects its own routes, so a 3xx comes from
    something in front of it, and the token must not follow it anywhere.
    """

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise RedirectRefused(
            "%s answered %d, a redirect to %s: not followed, so the token goes only "
            "to the URL the agent was given" % (req.full_url, code, newurl))


OPENER = urllib.request.build_opener(RefuseRedirects)


def request(url, token, method, path, body=None):
    """(status, parsed body)."""
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(url + path, data=data, method=method)
    req.add_header("Authorization", "Bearer " + token)
    if data is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with OPENER.open(req, timeout=10) as reply:
            return reply.status, json.loads(reply.read().decode() or "{}")
    except urllib.error.HTTPError as error:
        return error.code, json.loads(error.read().decode() or "{}")


def ids(run):
    """A trace id and a span-id prefix derived from the run id."""
    digest = hashlib.sha256(run.encode()).hexdigest()
    return digest[:32], int(digest[32:44], 16) << 8


def step_span(run, index, kind, name, status, start_ns):
    trace, base = ids(run)
    attributes = {"session.id": run}
    if kind == "llm":
        # gpt-6.1-sol lists at $2 input and $10 output per million tokens.
        tokens_in, tokens_out = 600 + 40 * index, 90
        attributes.update({"gen_ai.operation.name": "chat", "gen_ai.request.model": name,
                           "gen_ai.provider.name": "openai",
                           "gen_ai.usage.input_tokens": tokens_in,
                           "gen_ai.usage.output_tokens": tokens_out,
                           "llm.cost_usd": round((tokens_in * 2 + tokens_out * 10) / 1e6, 6)})
        span_name = "chat " + name
    else:
        attributes.update({"gen_ai.operation.name": "execute_tool", "gen_ai.tool.name": name,
                           "gen_ai.tool.call.arguments": json.dumps({"attempt": index})})
        span_name = "execute_tool " + name
    return {"trace_id": trace, "span_id": "%016x" % (base + index + 1), "parent_span_id": "%016x" % base,
            "name": span_name, "start_time_ns": start_ns, "end_time_ns": start_ns + 400_000_000,
            "status": status, "service": SERVICE, "attributes": attributes}


def root_span(run, start_ns, end_ns):
    """The run's root, exported when the ticket is done — as the corpus's are."""
    trace, base = ids(run)
    return {"trace_id": trace, "span_id": "%016x" % base, "parent_span_id": None,
            "name": "support.handle_ticket", "start_time_ns": start_ns, "end_time_ns": end_ns,
            "status": "ok", "service": SERVICE,
            "attributes": {"session.id": run, "traceloop.span.kind": "workflow",
                           "traceloop.entity.name": "handle_ticket", "session.outcome": "success"}}


def run(url, auth, run_id, branch, state_path, out, max_steps=None):
    script = BRANCHES[branch]
    try:
        with open(state_path) as handle:
            state = json.load(handle)
    except FileNotFoundError:
        state = {"done": 0, "started_ns": time.time_ns() - 5 * SECOND}
    taken = 0
    while state["done"] < len(script):
        if max_steps is not None and taken >= int(max_steps):
            return 4
        index = state["done"]
        kind, name, status = script[index]
        nxt = {"tool": name} if kind == "tool" else {}
        code, check = request(url, auth, "POST", "/v1/fuses/check", {"run": run_id, "next": nxt})
        if code != 200:
            raise SystemExit("the pre-call check answered %d: %s" % (code, check))
        with open(out + "check.json", "w") as handle:
            json.dump(check, handle)
        if check["verdict"] == "stop":
            return 3
        start = time.time_ns()
        code, ack = request(url, auth, "POST", "/v1/spans",
                            [step_span(run_id, index, kind, name, status, start)])
        if code != 200 or ack.get("accepted") != 1:
            raise SystemExit("ingest answered %d: %s" % (code, ack))
        state["done"] = index + 1
        with open(state_path, "w") as handle:
            json.dump(state, handle)
        code, fate = request(url, auth, "GET", "/v1/fate/runs/" + run_id)
        if code != 200:
            raise SystemExit("foresight answered %d: %s" % (code, fate))
        with open(out + "steps.jsonl", "a") as handle:
            handle.write(json.dumps({"step": index + 1, "kind": kind, "name": name, "status": status,
                                     "check": check, "fate": fate}) + "\n")
        taken += 1
    code, ack = request(url, auth, "POST", "/v1/spans",
                        [root_span(run_id, state["started_ns"], time.time_ns())])
    if code != 200:
        raise SystemExit("ingest answered %d: %s" % (code, ack))
    return 0


def ready(url, auth, run_id, seconds=15.0):
    """Waits until the run's verdict carries foresight from a cached map.

    The fuses read only cached Fate Maps and never build one; a check that
    finds none asks for a background build. The demo's map request in beat 2
    normally cached it already, so this returns at once — the wait is there
    for a slow machine, not for the demo's timing. A verdict read (no `next`)
    delivers nothing and changes nothing about the run.
    """
    deadline = time.time() + seconds
    while True:
        code, check = request(url, auth, "POST", "/v1/fuses/check", {"run": run_id})
        if code == 200 and (check.get("foresight") or {}).get("map_built_at_ns"):
            return 0
        if time.time() > deadline:
            print("agent: no cached Fate Map after %.0f s: %s" % (seconds, check), file=sys.stderr)
            return 1
        time.sleep(0.2)


USAGE = """usage: agent.py run URL TOKEN RUN BRANCH STATE OUT [MAX_STEPS]
       agent.py ready URL TOKEN RUN"""

if __name__ == "__main__":
    mode = sys.argv[1] if len(sys.argv) > 1 else ""
    if not (mode == "run" and len(sys.argv) in (8, 9) or mode == "ready" and len(sys.argv) == 5):
        sys.exit(USAGE)
    try:
        sys.exit(run(*sys.argv[2:]) if mode == "run" else ready(*sys.argv[2:]))
    except (RedirectRefused, urllib.error.URLError, OSError) as error:
        print("agent: %s" % error, file=sys.stderr)
        sys.exit(1)
