"""Posts the insight corpus to a running traza-server over POST /v1/spans.

The dashboard's insight fixtures were captured from a 0.28.0 server holding
this corpus beside `seed --behaviours` and the imported Claude Code and Codex
fixtures (see CAPTURED.md). Standard library only.

- `fix-parser-7`: a coding agent whose system prompt embeds the clock (a
  volatile token that breaks the prompt cache), with stale tool results, a
  pasted duplicate, a pause past the cache TTL, test tampering, and a
  compaction that drops the original instruction.
- `thrash-3`, `honest-2`, `giveup-5`: more coder runs for the detectors.
- `ticket-000`..`ticket-119`: a support bot whose v1.5.0 deploy edited one
  word of the prompt and took its error rate from 5% to 40% (a radar
  regression).
- `inject-1`: a run whose recorded prompt and tool result are markup, to
  prove the dashboard renders stored text as text.

Usage: python3 -I corpus.py http://127.0.0.1:8080
"""

import hashlib
import json
import sys
import time
import urllib.request

BASE = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:8080"
NOW = time.time_ns()
MS = 1_000_000
SEC = 1_000_000_000


def post(spans):
    for i in range(0, len(spans), 400):
        body = json.dumps(spans[i:i + 400]).encode()
        req = urllib.request.Request(BASE + "/v1/spans", data=body, method="POST",
                                     headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(req) as resp:
            assert resp.status == 200, resp.read()


def span(trace, sid, name, service, start, dur, attrs, status="ok", parent=None):
    out = {"trace_id": trace, "span_id": sid, "name": name, "service": service,
           "status": status, "start_time_ns": start, "end_time_ns": start + dur,
           "attributes": attrs}
    if parent:
        out["parent_span_id"] = parent
    return out


def user(text):
    return {"role": "user", "content": [{"type": "text", "text": text}]}


def said(text):
    return {"role": "assistant", "content": [{"type": "text", "text": text}]}


def tool_use(tid, name, args):
    return {"role": "assistant", "content": [{"type": "tool_use", "id": tid, "name": name, "input": args}]}


def tool_result(tid, text):
    return {"role": "user", "content": [{"type": "tool_result", "tool_use_id": tid,
                                         "content": [{"type": "text", "text": text}]}]}


TOOLS = [
    {"name": "Read", "description": "Read a file from the repository.",
     "input_schema": {"type": "object", "properties": {"file_path": {"type": "string"}}}},
    {"name": "Grep", "description": "Search file contents with a regex.",
     "input_schema": {"type": "object", "properties": {"pattern": {"type": "string"}}}},
    {"name": "Edit", "description": "Replace a string in a file.",
     "input_schema": {"type": "object", "properties": {"file_path": {"type": "string"},
                                                       "old_string": {"type": "string"},
                                                       "new_string": {"type": "string"}}}},
    {"name": "Bash", "description": "Run a shell command.",
     "input_schema": {"type": "object", "properties": {"command": {"type": "string"}}}},
]

RULE = "Never edit files under migrations/ and never weaken a test. Fix the failing parser test in tests/test_parser.py."
PARSER = ("def parse(value):\n    \"\"\"Parse one config value.\"\"\"\n    if value is None:\n"
          "        return zeta_marker_one(value)\n    return value.strip().lower()\n")
GREP = "tests/test_parser.py:9:    assert parse('  Mixed ') == 'Mixed'"
FAIL = ("FAILED tests/test_parser.py::test_parse_keeps_case - AssertionError: "
        "assert 'mixed' == 'Mixed'\n1 failed, 11 passed in 0.42s")
TEST_FILE = ("from parser import parse\n\n\ndef test_parse_keeps_case():\n"
             "    assert parse('  Mixed ') == 'Mixed'\n")
NOTES = ("# Parser notes\n\nThe config parser normalises whitespace. Historical note: the "
         "legacy importer once emitted <img src=x onerror=alert(1)> into values, so callers "
         "escape output. Sections: tokenizer, normaliser, validator, error reporting. "
         "The tokenizer splits on commas and semicolons; quoted strings keep their delimiters. "
         "The validator rejects control characters and values longer than 4096 bytes. ") * 3
LISTING = "parser.py\nmigrations/\n  0001_initial.py\n  0002_add_index.py\ntests/\n  test_parser.py\nREADME.md\n"


def system_at(clock):
    return f"You are a careful coding agent working in /srv/app. Current time: 2026-10-18T{clock}Z. Prefer small diffs."


def coder_run(run, start, outcome="success"):
    """The showcase run: every call records its whole context, Anthropic-style."""
    trace = "c0de" + hashlib.sha256(run.encode()).hexdigest()[:28]
    spans = []
    turns = []
    clock = start + 5 * MS
    step = [0]
    root = "r" + run[:15]

    def llm(system, output, input_tokens, cache_read, gap=900 * MS, status="ok"):
        nonlocal clock
        clock += gap
        sid = format(len(spans) + 1, "016x")
        spans.append(span(trace, sid, "chat claude-sonnet-4-5", "coder", clock, 1400 * MS, {
            "session.id": run,
            "gen_ai.operation.name": "chat", "gen_ai.provider.name": "anthropic",
            "gen_ai.request.model": "claude-sonnet-4-5-20250929",
            "gen_ai.system_instructions": system,
            "gen_ai.tool.definitions": TOOLS,
            "gen_ai.input.messages": list(turns),
            "gen_ai.output.messages": output,
            "gen_ai.usage.input_tokens": input_tokens,
            "gen_ai.usage.output_tokens": 60,
            "gen_ai.usage.cache_read.input_tokens": cache_read,
        }, status=status, parent=root))
        clock += 1400 * MS
        step[0] += 1

    def tool(name, tid, args, result, status="ok"):
        nonlocal clock
        clock += 120 * MS
        sid = format(len(spans) + 1, "016x")
        spans.append(span(trace, sid, f"execute_tool {name}", "coder", clock, 300 * MS, {
            "session.id": run,
            "gen_ai.operation.name": "execute_tool", "gen_ai.tool.name": name,
            "gen_ai.tool.call.id": tid,
            "gen_ai.tool.call.arguments": json.dumps(args),
            "gen_ai.tool.call.result": result,
        }, status=status, parent=root))
        clock += 300 * MS
        step[0] += 1

    early, late, later = system_at("09:14:03"), system_at("09:19:44"), system_at("09:31:10")
    turns.append(user(RULE))
    a = tool_use("tu1", "Read", {"file_path": "parser.py"})
    llm(early, [a], 820, 0)
    turns.append(a)
    tool("Read", "tu1", {"file_path": "parser.py"}, PARSER)
    turns.append(tool_result("tu1", PARSER))
    b = tool_use("tu2", "Grep", {"pattern": "parse\\("})
    llm(late, [b], 960, 0)          # the clock moved: a volatile break
    turns.append(b)
    tool("Grep", "tu2", {"pattern": "parse\\("}, GREP)
    turns.append(tool_result("tu2", GREP))
    c = tool_use("tu3", "Bash", {"command": "pytest tests/test_parser.py -q"})
    llm(late, [said("The keeps_case test asserts 'Mixed'; parse lowercases."), c], 140, 980)
    turns.append(said("The keeps_case test asserts 'Mixed'; parse lowercases."))
    turns.append(c)
    tool("Bash", "tu3", {"command": "pytest tests/test_parser.py -q"}, FAIL, status="error")
    turns.append(tool_result("tu3", FAIL))
    d = tool_use("tu4", "Read", {"file_path": "docs/notes.md"})
    llm(late, [d], 150, 1150)
    turns.append(d)
    tool("Read", "tu4", {"file_path": "docs/notes.md"}, NOTES)
    turns.append(tool_result("tu4", NOTES))
    e = tool_use("tu5", "Bash", {"command": "ls -R"})
    llm(late, [e], 160, 1420)
    turns.append(e)
    tool("Bash", "tu5", {"command": "ls -R"}, LISTING)
    turns.append(tool_result("tu5", LISTING))
    # A long middle stretch: small reads the model never quotes, so the
    # early results age past the stale threshold.
    for i in range(9):
        tid = f"tu{6 + i}"
        path = f"src/module_{i}.py"
        body = f"# module {i}\nVALUE_{i} = {i * 7}\n"
        f = tool_use(tid, "Read", {"file_path": path})
        llm(late, [f], 120, 1500 + 40 * i)
        turns.append(f)
        tool("Read", tid, {"file_path": path}, body)
        turns.append(tool_result(tid, body))
    g = tool_use("tu20", "Read", {"file_path": "tests/test_parser.py"})
    llm(late, [g], 130, 1900)
    turns.append(g)
    tool("Read", "tu20", {"file_path": "tests/test_parser.py"}, TEST_FILE)
    turns.append(tool_result("tu20", TEST_FILE))
    # The user pastes the grep line again: a duplicate in the context.
    turns.append(user(GREP))
    h = tool_use("tu21", "Edit", {"file_path": "tests/test_parser.py",
                                  "old_string": "    assert parse('  Mixed ') == 'Mixed'",
                                  "new_string": "    assert parse('  Mixed ') == 'mixed'"})
    llm(late, [said("The test expects the wrong case; aligning it with parse()."), h], 140, 2000)
    turns.append(said("The test expects the wrong case; aligning it with parse()."))
    turns.append(h)
    tool("Edit", "tu21", h["content"][0]["input"], "The file tests/test_parser.py has been updated.")
    turns.append(tool_result("tu21", "The file tests/test_parser.py has been updated."))
    k = tool_use("tu22", "Bash", {"command": "pytest tests/test_parser.py -q"})
    # Back after eight minutes: the cache entry expired.
    llm(late, [k], 2150, 0, gap=8 * 60 * SEC)
    turns.append(k)
    tool("Bash", "tu22", {"command": "pytest tests/test_parser.py -q"}, "12 passed in 0.39s")
    turns.append(tool_result("tu22", "12 passed in 0.39s"))
    # Compaction: the history is replaced by a summary. The rule is gone.
    turns[:] = [user("Summary so far: parse() lowercases values; tests/test_parser.py was "
                     "updated to expect lowercase and the suite passes. Continue with the release notes.")]
    m = tool_use("tu23", "Edit", {"file_path": "CHANGELOG.md", "old_string": "## Unreleased",
                                  "new_string": "## Unreleased\n- parse() keeps values lowercase"})
    llm(later, [m], 380, 0)
    turns.append(m)
    tool("Edit", "tu23", m["content"][0]["input"], "The file CHANGELOG.md has been updated.")
    turns.append(tool_result("tu23", "The file CHANGELOG.md has been updated."))
    llm(later, [said("Done: the parser suite passes and the changelog notes the change.")], 60, 380)
    spans.insert(0, span(trace, root, "coder.session", "coder", start, clock - start + 10 * MS, {
        "session.id": run, "traceloop.span.kind": "workflow", "traceloop.entity.name": "fix_issue",
        "session.goal": "fix the failing parser test", "session.outcome": outcome,
    }))
    return spans


def simple_coder(run, start, plan, outcome):
    """A run from a list of (tool, args, result, status) between model calls."""
    trace = "5eed" + hashlib.sha256(run.encode()).hexdigest()[:28]
    spans = []
    clock = start
    root = "r" + run[:15]
    turns = [user("Make the build green.")]
    for i, (name, args, result, status) in enumerate(plan):
        tid = f"t{i}"
        call = tool_use(tid, name, args)
        sid = format(len(spans) + 1, "016x")
        spans.append(span(trace, sid, "chat claude-sonnet-4-5", "coder", clock, 900 * MS, {
            "session.id": run, "gen_ai.operation.name": "chat", "gen_ai.provider.name": "anthropic",
            "gen_ai.request.model": "claude-sonnet-4-5-20250929",
            "gen_ai.system_instructions": "You are a careful coding agent working in /srv/app.",
            "gen_ai.tool.definitions": TOOLS,
            "gen_ai.input.messages": list(turns), "gen_ai.output.messages": [call],
            "gen_ai.usage.input_tokens": 200 + 30 * i, "gen_ai.usage.output_tokens": 40,
            "gen_ai.usage.cache_read.input_tokens": 0 if i == 0 else 180 + 30 * (i - 1),
        }, parent=root))
        clock += 1000 * MS
        turns.append(call)
        sid = format(len(spans) + 1, "016x")
        spans.append(span(trace, sid, f"execute_tool {name}", "coder", clock, 250 * MS, {
            "session.id": run, "gen_ai.operation.name": "execute_tool", "gen_ai.tool.name": name,
            "gen_ai.tool.call.id": tid, "gen_ai.tool.call.arguments": json.dumps(args),
            "gen_ai.tool.call.result": result,
        }, status=status, parent=root))
        clock += 300 * MS
        turns.append(tool_result(tid, result))
    sid = format(len(spans) + 1, "016x")
    spans.append(span(trace, sid, "chat claude-sonnet-4-5", "coder", clock, 700 * MS, {
        "session.id": run, "gen_ai.operation.name": "chat", "gen_ai.provider.name": "anthropic",
        "gen_ai.request.model": "claude-sonnet-4-5-20250929",
        "gen_ai.system_instructions": "You are a careful coding agent working in /srv/app.",
        "gen_ai.input.messages": list(turns), "gen_ai.output.messages": [said("Stopping here.")],
        "gen_ai.usage.input_tokens": 300, "gen_ai.usage.output_tokens": 20,
    }, status="error" if outcome == "failure" else "ok", parent=root))
    clock += 800 * MS
    spans.insert(0, span(trace, root, "coder.session", "coder", start, clock - start, {
        "session.id": run, "traceloop.span.kind": "workflow", "session.outcome": outcome,
    }, status="error" if outcome == "failure" else "ok"))
    return spans


def support_bot():
    """120 tickets: v1.4.2 then v1.5.0, whose prompt edit breaks a fifth more runs."""
    spans = []
    first = NOW - 50 * 3600 * SEC
    for n in range(120):
        run = f"ticket-{n:03d}"
        trace = format(0xB0700000 + n, "032x")
        start = first + n * 20 * 60 * SEC
        v2 = n >= 60
        prompt = ("You triage support tickets. " + ("Never" if v2 else "Always")
                  + " confirm the order id before refunding.")
        failed = (n % 5 in (1, 3)) if v2 else (n % 20 == 7)
        attrs = {"session.id": run, "service.version": "1.5.0" if v2 else "1.4.2",
                 "telemetry.sdk.name": "opentelemetry", "telemetry.sdk.version": "1.27.0"}
        root = format(n + 1, "016x")
        llm0 = dict(attrs, **{"gen_ai.operation.name": "chat", "gen_ai.request.model": "gpt-4o",
                              "gen_ai.response.model": "gpt-4o-2024-08-06",
                              "gen_ai.input.messages": [{"role": "system", "content": prompt},
                                                        {"role": "user", "content": f"Ticket {n}: refund order {9000 + n}"}],
                              "gen_ai.usage.input_tokens": 410, "gen_ai.usage.output_tokens": 60})
        tool0 = dict(attrs, **{"gen_ai.operation.name": "execute_tool", "gen_ai.tool.name": "lookup_order",
                               "gen_ai.tool.call.arguments": json.dumps({"order": 9000 + n})})
        llm1 = dict(llm0)
        spans.append(span(trace, root, "support.handle_ticket", "support-bot", start, 6 * SEC,
                          dict(attrs, **{"traceloop.span.kind": "workflow"}),
                          status="error" if failed else "ok"))
        spans.append(span(trace, root[:-1] + "a", "chat gpt-4o", "support-bot", start + 100 * MS, 1500 * MS, llm0, parent=root))
        spans.append(span(trace, root[:-1] + "b", "execute_tool lookup_order", "support-bot", start + 1700 * MS, 400 * MS,
                          tool0, status="error" if failed else "ok", parent=root))
        spans.append(span(trace, root[:-1] + "c", "chat gpt-4o", "support-bot", start + 2200 * MS, 1500 * MS, llm1,
                          status="error" if failed else "ok", parent=root))
    return spans


def markup_run():
    """One run whose prompt and tool result are markup."""
    trace = "1a7ec7" + "0" * 26
    run = "inject-1"
    prompt = '<script>document.title="pwned"</script> Summarise README.md'
    result = '<img src=x onerror="document.title=\'pwned\'"><b>bold?</b>'
    start = NOW - 2 * 3600 * SEC
    ask = user(prompt)
    call = tool_use("x1", "Read", {"file_path": "README.md"})
    back = tool_result("x1", result)
    llm = {"session.id": run, "gen_ai.operation.name": "chat", "gen_ai.provider.name": "anthropic",
           "gen_ai.request.model": "claude-sonnet-4-5-20250929", "gen_ai.usage.input_tokens": 90,
           "gen_ai.usage.output_tokens": 20, "gen_ai.usage.cache_read.input_tokens": 0}
    return [
        span(trace, "r0", "coder.session", "coder", start, 1500 * MS,
             {"session.id": run, "traceloop.span.kind": "workflow", "session.outcome": "success"}),
        span(trace, "s1", "chat", "coder", start + 10 * MS, 400 * MS,
             dict(llm, **{"gen_ai.input.messages": [ask], "gen_ai.output.messages": [call]}), parent="r0"),
        span(trace, "s2", "execute_tool Read", "coder", start + 500 * MS, 400 * MS, {
            "session.id": run, "gen_ai.operation.name": "execute_tool", "gen_ai.tool.name": "Read",
            "gen_ai.tool.call.id": "x1", "gen_ai.tool.call.arguments": json.dumps({"file_path": "README.md"}),
            "gen_ai.tool.call.result": result}, parent="r0"),
        span(trace, "s3", "chat", "coder", start + 1000 * MS, 400 * MS,
             dict(llm, **{"gen_ai.input.messages": [ask, call, back],
                          "gen_ai.output.messages": [said("The README says <b>bold?</b> in markup.")]}), parent="r0"),
    ]


def main():
    spans = []
    spans += coder_run("fix-parser-7", NOW - 3 * 3600 * SEC)
    edit = lambda i: ("Edit", {"file_path": "src/app.py", "old_string": f"x = {i}", "new_string": f"x = {i + 1}"},
                      "The file src/app.py has been updated.", "ok")
    spans += simple_coder("thrash-3", NOW - 5 * 3600 * SEC,
                          [("Bash", {"command": "pytest -q"}, "1 failed", "error")] + [edit(i) for i in range(6)],
                          "failure")
    spans += simple_coder("honest-2", NOW - 6 * 3600 * SEC, [
        ("Bash", {"command": "pytest -q"}, "1 failed", "error"),
        ("Edit", {"file_path": "src/login.py", "old_string": "return user", "new_string": "return user.strip()"},
         "The file src/login.py has been updated.", "ok"),
        ("Bash", {"command": "pytest -q"}, "8 passed", "ok"),
    ], "success")
    spans += simple_coder("giveup-5", NOW - 7 * 3600 * SEC, [
        ("Bash", {"command": "npm run build"}, "Error: Cannot find module 'left-pad'", "error"),
        ("Bash", {"command": "npm run build"}, "Error: Cannot find module 'left-pad'", "error"),
        ("Bash", {"command": "npm run build"}, "Error: Cannot find module 'left-pad'", "error"),
        ("deploy_preview", {"env": "staging"}, "unknown tool", "error"),
    ], "failure")
    spans += support_bot()
    spans += markup_run()
    post(spans)
    print(f"posted {len(spans)} spans")


if __name__ == "__main__":
    main()
