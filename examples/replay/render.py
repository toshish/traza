#!/usr/bin/env python3
"""Renders and asserts one step of the replay demo.

Kept out of the shell script for the reason the other demos keep theirs:
quoting JSON inside `python3 -c` is where scripts like this go wrong. Every
mode prints what happened and exits non-zero when it is not what the demo
claims.
"""

import json
import sys


def must(condition, message):
    if not condition:
        print("ASSERTION FAILED: " + message, file=sys.stderr)
        sys.exit(1)


def load(path):
    with open(path) as handle:
        return json.load(handle)


def describe(response):
    if response["tool_calls"]:
        return ", ".join("tool_call %s(%s) id=%s" % (c["name"], json.dumps(c["arguments"]), c["id"])
                         for c in response["tool_calls"])
    return json.dumps(response["content"], ensure_ascii=False)


def transcript(path):
    result = load(path)
    must(result["answer"] is not None, "the agent did not finish: %s" % result["refusal"])
    for n, response in enumerate(result["responses"], 1):
        print("    model call %d → %s" % (n, describe(response)))
        if response["tool_calls"]:
            print("    tool get_weather runs locally → its result goes back to the model")
    print("    answer: " + result["answer"])


def tape(path):
    doc = load(path)
    kinds = [step["kind"] for step in doc["steps"]]
    must(kinds == ["llm", "tool", "llm"], "expected llm, tool, llm on the tape, got %s" % kinds)
    for step in doc["steps"]:
        must("response" in step, "step %d recorded no response" % step["index"])
        print("    %d  %-4s  %-34s request %s…  response %s…" % (
            step["index"], step["kind"], step["fingerprint"],
            step["request_digest"][:12], step["response_digest"][:12]))
    print("    (%d spans examined; the workflow wrapper is context, not a step)"
          % doc["spans_examined"])


def count(path):
    try:
        with open(path) as handle:
            print(sum(1 for _ in handle))
    except FileNotFoundError:
        print(0)


def created(path):
    doc = load(path)
    must(doc["mode"] in ("strict", "loose"), "unexpected mode %r" % doc["mode"])
    must(doc["llm_steps"] == 2 and doc["steps"] == 3,
         "expected 3 steps, 2 of them model calls: %s" % doc)
    print("    %s · mode %s · %d steps, %d of them model calls"
          % (doc["id"], doc["mode"], doc["steps"], doc["llm_steps"]))
    print("    base_urls.openai: %s" % doc["base_urls"]["openai"])


def same(recorded_path, replayed_path, base):
    recorded, replayed = load(recorded_path), load(replayed_path)
    must(replayed["answer"] is not None, "the replayed agent did not finish: %s" % replayed["refusal"])
    must(len(recorded["responses"]) == len(replayed["responses"]),
         "different number of model answers: %d vs %d"
         % (len(recorded["responses"]), len(replayed["responses"])))
    for n, (a, b) in enumerate(zip(recorded["responses"], replayed["responses"]), 1):
        must(a == b, "model answer %d differs:\n  recorded %s\n  replayed %s" % (n, a, b))
        print("    model call %d: identical — %s" % (n, describe(b)))
    must(recorded["answer"] == replayed["answer"], "the final answers differ")
    print("    final answer: identical — %s" % json.dumps(replayed["answer"], ensure_ascii=False))
    calls = replayed["model_calls"]
    stray = [url for url in calls if not url.startswith(base)]
    must(not stray, "model calls went somewhere other than the replay: %s" % stray)
    print("    every one of the agent's %d model calls went to the replay:" % len(calls))
    print("      " + base + "/chat/completions")


def served(path, llm_steps):
    doc = load(path)
    must(doc["served"] == int(llm_steps), "served %d, expected %s" % (doc["served"], llm_steps))
    must(doc["remaining"] == 0, "%d recorded model steps were not asked for" % doc["remaining"])
    must(doc["diverged"] is None, "the replay recorded a divergence: %s" % doc["diverged"])
    must(all(call["matched"] for call in doc["calls"]), "a call did not match: %s" % doc["calls"])
    print("    served %d · remaining %d · diverged %s · calls %s"
          % (doc["served"], doc["remaining"], json.dumps(doc["diverged"]),
             json.dumps([{"step": c["step"], "matched": c["matched"]} for c in doc["calls"]])))


def refused(result_path, status_path):
    result, status = load(result_path), load(status_path)
    refusal = result["refusal"]
    must(refusal is not None, "the changed agent was served instead of refused")
    must(refusal["status"] == 409, "expected 409, got %s" % refusal["status"])
    error = refusal["body"]["error"]
    must(error["type"] == "replay_divergence", "expected replay_divergence: %s" % error)
    must(error["step"] == 0, "expected the divergence at step 0: %s" % error)
    must(refusal["x-should-retry"] == "false", "a divergence should carry x-should-retry: false")
    print("    HTTP %d · x-should-retry: %s" % (refusal["status"], refusal["x-should-retry"]))
    print("    " + json.dumps(refusal["body"], separators=(",", ":")))
    must(status["diverged"] is not None and status["diverged"]["step"] == 0,
         "the replay should remember the divergence at step 0: %s" % status["diverged"])
    must(status["served"] == 0 and status["remaining"] == 2,
         "a refused step must not be consumed: served %d, remaining %d"
         % (status["served"], status["remaining"]))
    print()
    print("    the replay afterwards: served %d · remaining %d · diverged at step %d"
          % (status["served"], status["remaining"], status["diverged"]["step"]))


def diff(path, expect):
    doc = load(path)
    for row in doc["aligned"]:
        print("    a[%s] b[%s]  %-8s %-6s %s" % (
            row["a"], row["b"], row["op"], row.get("why", ""),
            row["fingerprint_a"] if row["fingerprint_a"] == row["fingerprint_b"]
            else "%s → %s" % (row["fingerprint_a"], row["fingerprint_b"])))
    first = doc["first_divergence"]
    if expect == "same":
        must(first is None, "expected no divergence, got %s" % first)
        must(all(row["op"] == "same" for row in doc["aligned"]), "every step should be same")
        print("    first_divergence: null")
    else:
        must(first is not None, "the changed prompt should diverge")
        must(first["kind"] == "input" and first["a"] == 0 and first["b"] == 0,
             "expected an input divergence at step 0: %s" % first)
        print("    first_divergence: %s" % first["explanation"])
        print("    delta (b − a): %s" % json.dumps(doc["delta"], separators=(",", ":")))


MODES = {
    "transcript": transcript,
    "tape": tape,
    "count": count,
    "created": created,
    "same": same,
    "served": served,
    "refused": refused,
    "diff": diff,
}

if __name__ == "__main__":
    MODES[sys.argv[1]](*sys.argv[2:])
