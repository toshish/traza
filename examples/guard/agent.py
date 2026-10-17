#!/usr/bin/env python3
"""The guard demo's agent: a scripted research agent with a well-behaved harness.

The *model* is scripted — it asks for `fetch_page` until it has read the page,
then answers — so the demo needs no provider and no key. The *harness* is the
part under test, and it does what a real one should with run fuses:

  1. before each step, ask POST /v1/fuses/check whether the run may take it,
     with the step's estimated cost from the same pricing file the server
     uses, and halt on `stop`;
  2. after each step, export its spans and read the acknowledgement — the
     Traza-Fuse header and the `fuses` field — and halt on `stop`.

State lives in a JSON file, so the demo can stop the agent, act on the run
from outside (a kill switch, a reset, a grant), and start it again where it
left off. Exit codes: 0 finished, 3 halted by Traza, 4 paused at --max-steps,
1 anything else.

The other modes read one reply each and assert on it, kept out of run.sh for
the reason the other demos keep render.py: quoting JSON inside `python3 -c`
is where shell scripts like this go wrong.
"""

import hashlib
import json
import os
import sys
import time
import urllib.error
import urllib.request

MODEL = "sim-large"
SERVICE = "research-agent"
PAGE = "https://status.example.org/q3-report"
# A runaway agent with no fuse loops until something else stops it. This
# one stops itself here, so a regression in the fuse fails the demo instead
# of hanging it.
SELF_LIMIT = 20


def must(condition, message):
    if not condition:
        print("ASSERTION FAILED: " + message, file=sys.stderr)
        sys.exit(1)


def load(path):
    with open(path) as handle:
        return json.load(handle)


def save(path, value):
    with open(path, "w") as handle:
        json.dump(value, handle)


def request(url, token, method, path, body=None):
    """(status, Traza-Fuse header or None, parsed body, raw body)."""
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(url + path, data=data, method=method)
    req.add_header("Authorization", "Bearer " + token)
    if data is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=10) as reply:
            raw = reply.read().decode()
            return reply.status, reply.headers.get("Traza-Fuse"), json.loads(raw), raw
    except urllib.error.HTTPError as error:
        raw = error.read().decode() or "{}"
        return error.code, None, json.loads(raw), raw


# ------------------------------------------------------------ the model


def price(pricing, input_tokens, output_tokens):
    """USD, by the formula the server documents for --pricing."""
    rate = pricing["models"][MODEL]
    return (input_tokens * rate["input_per_mtok"] + output_tokens * rate["output_per_mtok"]) / 1e6


def usage_for(state):
    """The scripted model's token counts: fixed by the script, not a tokenizer.

    The prompt grows by 400 tokens for every tool result in the context —
    which is what makes a retry loop expensive: each failed attempt is resent
    with every attempt before it.
    """
    results = sum(1 for m in state["messages"] if m["role"] == "tool")
    return 1200 + 400 * results, 150


def text(role, content):
    return {"role": role, "parts": [{"type": "text", "content": content}]}


def decide(state):
    """The next assistant message: a tool call, or the answer."""
    task = state["task"]
    # Tool-call ids are "<plan id>-<attempt>"; a plan item is done once one of
    # its attempts returned without an error.
    answered = [m["parts"][0] for m in state["messages"] if m["role"] == "tool"]
    for call_id, tool, args in task["plan"]:
        attempts = [p for p in answered if p["id"].rsplit("-", 1)[0] == call_id]
        if not any("error" not in p["response"] for p in attempts):
            # A failed call is retried with the same arguments: the loop.
            return {"role": "assistant", "parts": [{
                "type": "tool_call", "id": "%s-%d" % (call_id, len(attempts)), "name": tool,
                "arguments": args}]}, (call_id, tool, args)
    return text("assistant", task["answer"]), None


def run_tool(tool, args, upstream):
    if tool == "fetch_page" and upstream == "down":
        return {"error": "HTTP 503 Service Unavailable from %s" % args["url"]}
    if tool == "search":
        return {"results": [PAGE, "https://example.org/q3-coverage"]}
    return {"status": 200, "text": "Q3 revenue grew 12%%; churn fell to 2.1%%. (%s)" % args["url"]}


TASKS = {
    "summarise": {
        "goal": "Summarise the Q3 status report at %s." % PAGE,
        "plan": [("call_fetch", "fetch_page", {"url": PAGE})],
        "answer": "Q3: revenue up 12%, churn down to 2.1%.",
    },
    "research": {
        "goal": "Compare the Q3 report with the press coverage.",
        "plan": [
            ("call_search", "search", {"q": "Q3 report coverage"}),
            ("call_report", "fetch_page", {"url": PAGE}),
            ("call_press", "fetch_page", {"url": "https://example.org/q3-coverage"}),
        ],
        "answer": "The coverage matches the report: revenue up 12%, churn 2.1%.",
    },
}


# ---------------------------------------------------------- the harness


def agent(argv):
    """agent URL TOKEN RUN TASK STATE PRICING UPSTREAM MAX_STEPS OUT_PREFIX"""
    url, token, run, task_name, state_path, pricing_path, upstream, max_steps, out = argv
    pricing = load(pricing_path)
    if os.path.exists(state_path):
        state = load(state_path)
    else:
        task = TASKS[task_name]
        state = {
            "run": run, "task": task, "step": 0, "cost_usd": 0.0, "tool_spans": [],
            "trace_id": hashlib.sha256(run.encode()).hexdigest()[:32],
            "messages": [
                text("system", "You are a research agent. Read sources with tools before answering."),
                text("user", task["goal"]),
            ],
        }
    taken = 0
    while True:
        if taken >= int(max_steps):
            save(state_path, state)
            print("    (paused after %d step(s); the run is left as it stands)" % taken)
            return 4
        must(state["step"] < SELF_LIMIT, "no fuse stopped the agent in %d steps" % SELF_LIMIT)

        # 1 · the pre-call gate, before anything is spent.
        input_tokens, output_tokens = usage_for(state)
        estimate = price(pricing, input_tokens, output_tokens)
        status, _, verdict, _ = request(url, token, "POST", "/v1/fuses/check",
                                     {"run": run, "next": {"est_cost_usd": round(estimate, 8)}})
        must(status == 200, "the fuse check answered %d: %s" % (status, verdict))
        save(out + "check.json", verdict)
        if verdict["verdict"] == "stop":
            save(state_path, state)
            print("    step %d  gate: stop — the step is never taken" % (state["step"] + 1))
            for reason in verdict["reasons"]:
                print("      reason: " + reason)
            return 3

        # 2 · the step: one model call, and the tool it asks for.
        state["step"] += 1
        n = state["step"]
        reply, call = decide(state)
        # The scripted model's usage is fixed, so the step costs exactly what
        # the gate was told it would.
        cost = estimate
        state["cost_usd"] += cost
        start = time.time_ns()
        spans = [{
            "trace_id": state["trace_id"], "span_id": "llm-%02d" % n,
            "name": "chat " + MODEL, "service": SERVICE, "status": "ok",
            "start_time_ns": start, "end_time_ns": start + 1,
            "attributes": {
                "session.id": run,
                "gen_ai.operation.name": "chat",
                "gen_ai.request.model": MODEL,
                "gen_ai.usage.input_tokens": input_tokens,
                "gen_ai.usage.output_tokens": output_tokens,
                "gen_ai.input.messages": json.dumps(state["messages"]),
                "gen_ai.output.messages": json.dumps([reply]),
            },
        }]
        state["messages"].append(reply)
        line = "    step %d  %s %d in / %d out  $%.5f" % (n, MODEL, input_tokens, output_tokens, cost)
        if call is not None:
            _, tool, args = call
            result = run_tool(tool, args, upstream)
            part = reply["parts"][0]
            state["messages"].append({"role": "tool", "parts": [{
                "type": "tool_call_response", "id": part["id"], "response": result}]})
            failed = "error" in result
            tool_span = {
                "trace_id": state["trace_id"], "span_id": "tool-%02d" % n,
                "name": "execute_tool " + tool, "service": SERVICE,
                "status": "error" if failed else "ok",
                "start_time_ns": start + 2, "end_time_ns": time.time_ns() + 3,
                "attributes": {
                    "session.id": run,
                    "gen_ai.operation.name": "execute_tool",
                    "gen_ai.tool.name": tool,
                    "gen_ai.tool.call.id": part["id"],
                    "gen_ai.tool.call.arguments": json.dumps(args),
                    "gen_ai.tool.call.result": json.dumps(result),
                },
            }
            if failed:
                tool_span["attributes"]["error.message"] = result["error"]
            spans.append(tool_span)
            state["tool_spans"].append(tool_span["span_id"])
            line += "  → %s(%s) %s" % (tool, json.dumps(args), "FAILED: 503" if failed else "ok")
        else:
            line += "  → answer"

        # 3 · export, and listen to the acknowledgement.
        status, header, ack, raw = request(url, token, "POST", "/v1/spans", spans)
        must(status == 200, "ingest answered %d: %s" % (status, ack))
        must(ack.get("accepted") == len(spans), "not every span was stored: %s" % ack)
        mine = [f for f in ack.get("fuses", []) if f["run"] == run]
        verdict = mine[0]["verdict"] if mine else "continue"
        print(line + "  · ack: " + verdict)
        if verdict == "stop":
            save(state_path, state)
            save(out + "ack.json", {"header": header, "body": ack})
            print()
            print("    the acknowledgement of step %d, verbatim:" % n)
            print("      Traza-Fuse: %s" % header)
            print("      %s" % raw)
            return 3
        if call is None:
            state["done"] = True
            save(state_path, state)
            print()
            print("    answer: " + state["task"]["answer"])
            return 0
        taken += 1


# ------------------------------------------------------- reading replies


def field(path, *keys):
    """Print one field of a JSON file, by key path (integers index lists)."""
    value = load(path)
    for key in keys:
        value = value[int(key)] if isinstance(value, list) else value[key]
    print(value if not isinstance(value, (dict, list)) else json.dumps(value))


def policy(path):
    doc = load(path)
    must(doc.get("action") == "stop", "the policy should stop: %s" % doc)
    print("  " + json.dumps(doc, separators=(",", ":")))


def stopped_on_ack(ack_path, state_path, run, repeats):
    ack, state = load(ack_path), load(state_path)
    must(ack["header"] is not None and ack["header"].startswith("stop; run=" + run),
         "the ack should carry Traza-Fuse: stop; run=%s, got %r" % (run, ack["header"]))
    fuses = [f for f in ack["body"]["fuses"] if f["run"] == run]
    must(fuses and fuses[0]["verdict"] == "stop", "the ack's fuses field should say stop")
    must(any(r.startswith("max_repeats %s reached" % repeats) for r in fuses[0]["reasons"]),
         "the stop should name max_repeats %s: %s" % (repeats, fuses[0]["reasons"]))
    must(state["step"] == int(repeats),
         "the loop should stop at its step %s, it took %d" % (repeats, state["step"]))


def verdict(path, state_path, url, token, repeats):
    """The run's verdict document: reasons, usage, and the evidence followed."""
    doc, state = load(path), load(state_path)
    must(doc["verdict"] == "stop" and doc["tripped_by"] == "policy",
         "expected a policy stop, got %s by %s" % (doc["verdict"], doc["tripped_by"]))
    usage = doc["usage"]
    print("  verdict:  %s   (tripped_by %s, policy %r, source %s)"
          % (doc["verdict"], doc["tripped_by"], doc["policy"], doc["source"]))
    for reason in doc["reasons"]:
        print("  reason:   " + reason)
    print("  usage:    %d steps (%d llm, %d tool) · %d errors · %d tokens · $%.5f"
          % (usage["steps"], usage["llm_calls"], usage["tool_calls"], usage["errors"],
             usage["tokens"], usage["cost_usd"]))
    top = usage["top_repeat"]
    must(top is not None and top["count"] == int(repeats),
         "top_repeat should count %s, got %s" % (repeats, top))
    print("  repeat:   %s × %d" % (top["fingerprint"], top["count"]))
    # The cost is the server's own, priced from --pricing; the agent priced
    # each call from the same file. They must agree.
    must(abs(usage["cost_usd"] - state["cost_usd"]) < 1e-9,
         "server cost $%s != the agent's own $%s" % (usage["cost_usd"], state["cost_usd"]))
    print("  cost:     $%.5f priced by the server from --pricing; the agent's own"
          % usage["cost_usd"])
    print("            sum from the same file is $%.5f — equal (asserted)" % state["cost_usd"])
    must(doc["evidence"], "a stop should carry evidence")
    print("  evidence:")
    statuses = {}
    for item in doc["evidence"]:
        status, _, trace, _ = request(url, token, "GET", "/v1/traces/" + item["trace_id"])
        must(status == 200, "evidence trace unreadable: %d" % status)
        span = [s for s in trace["spans"] if s["span_id"] == item["span_id"]]
        must(span, "evidence span %s is not in its trace" % item["span_id"])
        span = span[0]
        statuses[item["span_id"]] = span["status"]
        note = span["attributes"].get("error.message") or span["attributes"].get(
            "gen_ai.request.model", "")
        print("    %s  %-22s status %-5s %s" % (item["span_id"], span["name"], span["status"], note))
    # The stop's evidence is the span that crossed the limit — the last
    # fetch_page the loop recorded — and it reads back as the failure it was.
    last = state["tool_spans"][-1]
    must(statuses.get(last) == "error",
         "the evidence should be the loop's last tool span %s, status error: %s" % (last, statuses))
    print("            %s is the loop's last fetch_page, read back failed (asserted)" % last)


def gate(path, kind):
    doc = load(path)
    must(doc["verdict"] == "stop", "the gate should refuse, it said %s" % doc["verdict"])
    if kind == "hypothetical":
        # The check changed nothing: a stop it predicts is not a recorded one.
        must(doc["tripped_by"] is None, "a hypothetical stop records nothing: %s" % doc)
    joined = " ".join(doc["reasons"])
    wanted = {"sticky": "max_repeats", "manual": "manually tripped",
              "hypothetical": "would be exceeded by the next step"}[kind]
    must(wanted in joined, "expected %r among the reasons: %s" % (wanted, doc["reasons"]))
    print("    the check answered, verbatim:")
    print("      " + json.dumps(doc, separators=(",", ":")))


def control(path, expect_verdict, *expect_grant):
    doc = load(path)
    must(doc["verdict"] == expect_verdict,
         "expected %s, got %s: %s" % (expect_verdict, doc["verdict"], doc["reasons"]))
    print("    verdict %s · tripped_by %s · grant %s · limits %s"
          % (doc["verdict"], json.dumps(doc["tripped_by"]), json.dumps(doc["grant"]),
             json.dumps(doc["limits"], separators=(",", ":"))))
    for reason in doc["reasons"]:
        print("    reason: " + reason)
    if expect_grant:
        key, value = expect_grant[0].split("=")
        must(doc["grant"] and doc["grant"].get(key) == float(value),
             "the grant should set %s=%s: %s" % (key, value, doc["grant"]))


def finished(path, state_path, cap, grant):
    doc, state = load(path), load(state_path)
    must(state.get("done"), "the agent should have finished its task")
    cost = doc["usage"]["cost_usd"]
    must(doc["verdict"] != "stop", "the run should be allowed on: %s" % doc["reasons"])
    must(cost > float(cap), "the run should have spent past the policy's $%s, spent $%s" % (cap, cost))
    must(cost <= float(grant), "the run spent past its grant: $%s" % cost)
    must(abs(cost - state["cost_usd"]) < 1e-9, "server and agent disagree on cost")
    print("    final: verdict %s · $%.5f spent — past the policy's $%s, within the $%s grant"
          % (doc["verdict"], cost, cap, grant))


def hook_result(code, stderr_path, want_code, *want_text):
    code, want_code = int(code), int(want_code)
    with open(stderr_path) as handle:
        err = handle.read()
    must(code == want_code, "the hook exited %d, wanted %d (stderr: %s)" % (code, want_code, err))
    for wanted in want_text:
        must(wanted in err, "the hook's stderr should contain %r: %s" % (wanted, err))
    return err


def hook(code, stderr_path, want_code, *want_text):
    err = hook_result(code, stderr_path, want_code, *want_text)
    print("exit %s" % code)
    for line in err.splitlines():
        print("        stderr: " + line)


def events(path, hook_run):
    """The feed must hold every transition the beats above caused, in the
    order they caused them, and nothing else."""
    doc = load(path)
    for event in doc["events"]:
        print("  %-11s %-16s %s" % (event["kind"], event["run"], "; ".join(event["reasons"])))
    got = [(e["kind"], e["run"]) for e in doc["events"]]
    wanted = [
        ("stop", "sess-loop"),            # beat 2: max_repeats
        ("manual_stop", "sess-healthy"),  # beat 5: the kill switch
        ("reset", "sess-loop"),           # beat 6: the plain reset...
        ("warn", "sess-loop"),            # ...which leaves 87% of the budget spent
        ("reset", "sess-loop"),           # beat 6: the reset with a grant
        ("stop", hook_run),               # beat 7: the hook's session
    ]
    must(got == wanted, "the event feed should be exactly %s, in order: %s" % (wanted, got))
    grant = doc["events"][4]["reasons"]
    must(any("grant" in reason for reason in grant), "the second reset should carry its grant: %s" % grant)
    print()
    print("  six transitions, in the order the beats above caused them (asserted)")


MODES = {
    "field": field,
    "policy": policy,
    "stopped-on-ack": stopped_on_ack,
    "verdict": verdict,
    "gate": gate,
    "control": control,
    "finished": finished,
    "hook": hook,
    "events": events,
}

if __name__ == "__main__":
    if sys.argv[1] == "agent":
        sys.exit(agent(sys.argv[2:]))
    MODES[sys.argv[1]](*sys.argv[2:])
