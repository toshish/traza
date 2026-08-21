#!/usr/bin/env python3
"""Renders one reply from stdin, in the shape this incident demo wants.

Two kinds of reply pass through here: a JSON-RPC envelope from POST /v1/mcp
(a `result` or an `error`), and a bare JSON object from a plain /v1 HTTP route.
The mode named in argv[1] says which, and what to pull out of it.

Kept out of the shell for the same reason mcp-demo keeps it out: escaping
double quotes inside a single-quoted `python3 -c` is where this goes wrong.
"""

import json
import sys


def load():
    return json.load(sys.stdin)


def indent(text, prefix="  "):
    for line in text.splitlines():
        print(prefix + line if line.strip() else "")


def result_text(message):
    """The text block of a tool result, whatever the mode wants to do with it."""
    return message["result"]["content"][0]["text"]


def error_text(err):
    """The message out of an `error` member.

    A JSON-RPC error is an object with a `message`; an HTTP-level refusal from
    a plain route is `{"error": "<string>"}`. Print the server's words either
    way rather than a traceback about the difference.
    """
    if isinstance(err, dict):
        return str(err.get("message", err))
    return str(err)


# ------------------------------------------------------------------ display

def tool(message):
    """A tool result: its text block, marked if it came back as an error."""
    if "error" in message:
        print("  error: " + error_text(message["error"]))
        return
    result = message["result"]
    indent(result["content"][0]["text"])
    if result.get("isError"):
        print()
        print("  [isError: the tool answered, and the answer is a refusal]")


def initialize(message):
    result = message["result"]
    print("  protocol: " + result["protocolVersion"])
    print("  server:   " + result["serverInfo"]["name"] + " " + result["serverInfo"]["version"])
    print("  offers:   " + ", ".join(sorted(result["capabilities"])))


def tools(message):
    for entry in message["result"]["tools"]:
        print("  {:<28} {}".format(entry["name"], entry["title"]))


def first_line(message):
    print("    " + result_text(message).splitlines()[0])


def rpc_error(message):
    print("    " + error_text(message["error"]))


# ------------------------------------------------------- machine extraction

def sessions(message):
    """One line per session: id, tokens, errors, span_count — tab separated.

    In the order the tool returned them, which for order_by=tokens is the
    ranking. The shell iterates these to find the runaway without ever
    hardcoding an id.
    """
    for row in message["result"]["structuredContent"]["sessions"]:
        print("{}\t{}\t{}\t{}".format(
            row["session_id"],
            row.get("total_tokens", 0),
            row.get("error_count", 0),
            row.get("span_count", 0),
        ))


FAULT_SHAPES = {"retry_storm", "context_runaway", "self_similar_chain", "declared_retry"}


def diag(message):
    """Key=value facts from a diagnosis, for the shell to branch on."""
    sc = message["result"]["structuredContent"]
    findings = sc.get("findings", [])
    faults = [f for f in findings if f.get("shape") in FAULT_SHAPES]
    runaway = next((f for f in findings if f.get("shape") == "context_runaway"), None)
    iteration = next((f for f in findings if f.get("shape") == "iteration"), None)
    outcome = sc.get("outcome", {})
    cause = sc.get("cause")
    print("findings_found={}".format(sc.get("findings_found", len(findings))))
    print("fault_findings={}".format(len(faults)))
    print("cause_present={}".format(1 if cause else 0))
    print("outcome={}".format(outcome.get("outcome", "")))
    print("outcome_reason={}".format(outcome.get("reason", "")))
    print("error_count={}".format(outcome.get("error_count", 0)))
    if cause:
        print("cause_trace={}".format(cause.get("span", {}).get("trace_id", "")))
        print("cause_span={}".format(cause.get("span", {}).get("span_id", "")))
        print("cause_name={}".format(cause.get("name", "")))
    if runaway:
        print("runaway_present=1")
        print("context_first={}".format(runaway.get("context_first", "")))
        print("context_last={}".format(runaway.get("context_last", "")))
        print("token_trend={}".format(runaway.get("token_trend", "")))
        print("runaway_count={}".format(runaway.get("count", "")))
        print("runaway_errors={}".format(runaway.get("error_count", "")))
    else:
        print("runaway_present=0")
    if iteration:
        print("iteration_count={}".format(iteration.get("count", "")))
        print("iteration_errors={}".format(iteration.get("error_count", "")))
        print("iteration_trend={}".format(iteration.get("token_trend", "")))


def promote(message):
    """dataset_id/version_id/examples/created from a promotion result."""
    sc = message["result"]["structuredContent"]
    print("dataset_id={}".format(sc.get("dataset_id", "")))
    print("version_id={}".format(sc.get("version_id", "")))
    print("examples={}".format(sc.get("examples", "")))
    print("created={}".format(str(sc.get("created", "")).lower()))


def has(message):
    """Print 'yes' when the tool's text block contains the needle in argv[2]."""
    needle = sys.argv[2]
    print("yes" if needle in result_text(message) else "no")


# ------------------------------------------------ plain HTTP JSON (not JSON-RPC)

def _walk(obj, path):
    for part in path.split("."):
        if part == "":
            continue
        if part.endswith("]") and "[" in part:
            key, idx = part[:-1].split("[", 1)
            if key:
                obj = obj[key]
            obj = obj[int(idx)]
        else:
            obj = obj[part]
    return obj


def get(message):
    """A dotted path into a bare JSON object: get 'scores[0].mean_base'."""
    value = _walk(message, sys.argv[2])
    if isinstance(value, (dict, list)):
        print(json.dumps(value, separators=(",", ":")))
    else:
        print(value)


def example_ids(message):
    """The example ids in a dataset version manifest, one per line."""
    for body in message["bodies"]:
        print(body["example_id"])


def diff(message):
    """The experiment-over-experiment diff, per score name, for display."""
    for row in message["scores"]:
        arrow = "{:.2f} -> {:.2f}".format(row["mean_base"], row["mean_candidate"])
        print("  {:<12} {}   (delta {:+.2f})".format(row["name"], arrow, row["delta"]))
        if row["improved"]:
            print("    improved: " + ", ".join(row["improved"]))
        if row["regressed"]:
            print("    regressed: " + ", ".join(row["regressed"]))


def diff_improved(message):
    """Every improved example id across every score name, deduped, one per line."""
    seen = []
    for row in message["scores"]:
        for example in row["improved"]:
            if example not in seen:
                seen.append(example)
    for example in seen:
        print(example)


MODES = {
    "tool": tool,
    "initialize": initialize,
    "tools": tools,
    "first-line": first_line,
    "rpc-error": rpc_error,
    "sessions": sessions,
    "diag": diag,
    "promote": promote,
    "has": has,
    "get": get,
    "example-ids": example_ids,
    "diff": diff,
    "diff-improved": diff_improved,
}

if __name__ == "__main__":
    MODES[sys.argv[1]](load())
