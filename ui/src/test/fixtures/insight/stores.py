"""Builds the insight fixtures' stores of their own, and captures them.

Each store goes to a fresh, empty traza-server, so the case it shows is the
only thing in it (see CAPTURED.md for the flags each server needs). The
script posts the store's spans over POST /v1/spans, makes the requests its
fixtures came from, and writes each response body unedited into OUTDIR.

- `blame`: three Edits of one line, one failed, one with no result, one
  applied (`blame_unconfirmed.json`).
- `params`: `tuned-bot`'s sampling parameters change type and spelling,
  `triage-bot`'s prompt gains a word (`changes_params.json`).
- `long`: three calls past the 2,000-chunk bound (`cache_long.json`).
- `unread`: a call whose request was offloaded, read with resolve=false
  (`xray_unread.json`); the server runs with --payload-threshold-bytes 200.
- `loops`: loop-agent's and mini-agent's runs (`fate_map_loops.json`,
  `fate_map_mini.json`, `tql_loops.json`).
- `tenants`: support-agent in two tenants (`changes_tenants.json`).
- `removed`: the live stream's evicted and expired runs
  (`live_stream_removed.txt`, kept in ui/src/test/fixtures); the server runs
  with --fuse-max-runs 1 --live-window-seconds 3.

The `loops` and `tenants` stores meter each model call by its tokens at its
model's list price; the others record no usage. Standard library only.

Usage: python3 -I stores.py STORE http://127.0.0.1:8080 OUTDIR
"""

import hashlib
import json
import re
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

STORE = sys.argv[1]
BASE = sys.argv[2]
OUT = sys.argv[3]
NOW = time.time_ns()
MS = 1_000_000
SEC = 1_000_000_000
MIN = 60 * SEC

# USD per million tokens, input and output, as the vendors list them
# (2026-10-18).
PRICES = {"gpt-6.1-sol": (2.00, 10.00), "gpt-6-luna": (0.10, 0.50)}


def post(spans):
    for i in range(0, len(spans), 400):
        body = json.dumps(spans[i:i + 400]).encode()
        req = urllib.request.Request(BASE + "/v1/spans", data=body, method="POST",
                                     headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(req) as resp:
            assert resp.status == 200, resp.read()


def get(path, params=None):
    url = BASE + path + ("?" + urllib.parse.urlencode(params, doseq=True) if params else "")
    try:
        with urllib.request.urlopen(url, timeout=120) as resp:
            return resp.status, resp.read()
    except urllib.error.HTTPError as err:
        return err.code, err.read()


def save(name, path, params=None):
    status, body = get(path, params)
    assert status == 200, (name, status, body)
    with open(f"{OUT}/{name}", "wb") as f:
        f.write(body)
    print(f"{status} {name} {path} {params or ''}", flush=True)
    return json.loads(body)


def span(trace, sid, name, service, start, dur, attrs, status="ok", parent=None, tenant=None):
    out = {"trace_id": trace, "span_id": sid, "name": name, "service": service,
           "start_time_ns": start, "end_time_ns": start + dur, "attributes": attrs}
    if status is not None:
        out["status"] = status
    if parent:
        out["parent_span_id"] = parent
    if tenant:
        out["$tenant"] = tenant
    return out


def metered(model, input_tokens, output_tokens):
    """A model call's usage, and its cost at the model's list price."""
    per_in, per_out = PRICES[model]
    return {"gen_ai.usage.input_tokens": input_tokens, "gen_ai.usage.output_tokens": output_tokens,
            "llm.cost_usd": round((input_tokens * per_in + output_tokens * per_out) / 1e6, 8)}


def steps_run(trace, run, service, start, steps, outcome, extra=None, tenant=None):
    """A run under a workflow root: each step 500 ms, back to back, 10 ms in.
    A step is ("llm", model) or ("tool", name)."""
    extra = extra or {}
    root = "r" + hashlib.sha256(run.encode()).hexdigest()[:15]
    failed = outcome == "failure"
    spans = []
    clock = start + 10 * MS
    for i, (kind, name) in enumerate(steps):
        sid = format(i + 1, "016x")
        if kind == "llm":
            attrs = dict(extra, **{"session.id": run, "gen_ai.operation.name": "chat",
                                   "gen_ai.request.model": name}, **metered(name, 300, 40))
            title = f"chat {name}"
        else:
            attrs = dict(extra, **{"session.id": run, "gen_ai.operation.name": "execute_tool",
                                   "gen_ai.tool.name": name})
            title = f"execute_tool {name}"
        last = i == len(steps) - 1
        spans.append(span(trace, sid, title, service, clock, 500 * MS, attrs,
                          status="error" if failed and last else "ok", parent=root, tenant=tenant))
        clock += 500 * MS
    spans.insert(0, span(trace, root, f"{service}.run", service, start, clock - start,
                         dict(extra, **{"session.id": run, "traceloop.span.kind": "workflow",
                                        "session.outcome": outcome}),
                         status="error" if failed else "ok", tenant=tenant))
    return spans


def blame():
    """Three Edits writing one line into one file, a run each."""
    line = "let total = items.len();"
    args = json.dumps({"file_path": "/srv/shop/src/cart.rs", "old_string": "let total = 0;", "new_string": line})
    cases = [
        ("session-failed", "error",
         "<tool_use_error>String to replace not found in file.\nString: let total = 0;</tool_use_error>"),
        ("session-cut-short", None, None),
        ("session-applied", "ok", "The file /srv/shop/src/cart.rs has been updated."),
    ]
    spans = []
    for n, (run, status, result) in enumerate(cases, start=1):
        attrs = {"session.id": run, "gen_ai.operation.name": "execute_tool", "gen_ai.tool.name": "Edit",
                 "gen_ai.tool.call.id": f"toolu_0{n}", "gen_ai.tool.call.arguments": args}
        if result is not None:
            attrs["gen_ai.tool.call.result"] = result
        spans.append(span(format(0xB1A3E000 + n, "032x"), format(0xF00 + n, "016x"), "execute_tool Edit",
                          "claude-code", NOW - 60 * SEC + n * SEC, 200 * MS, attrs, status=status))
    post(spans)
    save("blame_unconfirmed.json", "/v1/blame", {"path": "src/cart.rs", "line": line})


def params():
    """tuned-bot's parameters, then triage-bot's prompt, a run a minute."""
    spans = []
    for n in range(120):
        after = n >= 60
        attrs = {"session.id": f"tuned-{n:03d}", "gen_ai.operation.name": "chat",
                 "gen_ai.request.model": "gpt-6.1-sol", "gen_ai.response.model": "gpt-6.1-sol",
                 "gen_ai.input.messages": [{"role": "system", "content": "You answer billing questions."},
                                           {"role": "user", "content": f"Question {n}"}]}
        if after:
            attrs.update({"gen_ai.request.max_tokens": 1024, "gen_ai.request.stop": "END",
                          "gen_ai.request.temperature": 0.7})
        else:
            attrs.update({"gen_ai.request.max_tokens": "1024", "gen_ai.request.stop": "END, temperature=0.7"})
        failed = after and n % 5 in (1, 3)
        spans.append(span(format(0x7E5ED000 + n, "032x"), "00000000000000a1", "chat gpt-6.1-sol", "tuned-bot",
                          NOW - (130 - n) * MIN, SEC, attrs, status="error" if failed else "ok"))
    for n in range(120):
        after = n >= 60
        run = f"triage-{n:03d}"
        trace = format(0x7A1A6E00 + n, "032x")
        start = NOW - (130 - n) * MIN + 30 * SEC
        ended = n % 60 % 8 == 0
        outcome = ("failure" if after else "success") if ended else "escalated"
        prompt = "Triage the ticket briefly." if after else "Triage the ticket."
        spans.append(span(trace, "00000000000000b0", "triage.handle", "triage-bot", start, 2 * SEC,
                          {"session.id": run, "traceloop.span.kind": "workflow", "session.outcome": outcome}))
        spans.append(span(trace, "00000000000000b1", "chat gpt-6.1-sol", "triage-bot", start + 500 * MS, SEC, {
            "session.id": run, "gen_ai.operation.name": "chat", "gen_ai.request.model": "gpt-6.1-sol",
            "gen_ai.response.model": "gpt-6.1-sol",
            "gen_ai.input.messages": [{"role": "system", "content": prompt},
                                      {"role": "user", "content": f"Ticket {n}: my order never came"}]},
            parent="00000000000000b0"))
    post(spans)
    save("changes_params.json", "/v1/changes")


def long_context():
    """2,000 one-line turns; an instruction put ahead of them; one more turn."""
    turns = [{"role": "user", "content": f"turn {i}"} for i in range(2000)]
    ahead = [{"role": "user", "content": "A new instruction placed ahead of the whole history, "
                                         "far longer than any turn, so the prefix breaks at its first chunk."}]
    contexts = [turns, ahead + turns, ahead + turns + [{"role": "user", "content": "turn 2000"}]]
    trace = "10a9c" + "0" * 27
    spans = []
    for i, (messages, start) in enumerate(zip(contexts, (0, 10, 21))):
        spans.append(span(trace, format(0xC0 + i, "016x"), "chat claude-sonnet-5-5", "long-agent",
                          NOW - 3600 * SEC + start * SEC, SEC, {
                              "session.id": "long-ctx-1", "gen_ai.operation.name": "chat",
                              "gen_ai.provider.name": "anthropic", "gen_ai.request.model": "claude-sonnet-5-5",
                              "gen_ai.input.messages": messages,
                              "gen_ai.output.messages": [{"role": "assistant", "content": "Noted."}]}))
    post(spans)
    save("cache_long.json", "/v1/cache/long-ctx-1")


def unread():
    """Two calls of one conversation; the second's request is offloaded."""
    rules = [{"role": "system", "content": "Be brief."}, {"role": "user", "content": "Never touch migrations"}]
    question = {"role": "user", "content": "Which of the parser's three normalisation passes should run "
                                           "first when a value holds both quoted delimiters and trailing "
                                           "whitespace, and why does the order matter for the round trip?"}
    trace = "0ff10ad" + "0" * 25
    spans = []
    for i, messages in enumerate([rules, rules + [question]]):
        spans.append(span(trace, format(0xD0 + i, "016x"), "chat claude-sonnet-5-5", "unread-agent",
                          NOW - 600 * SEC + i * 5 * SEC, SEC, {
                              "session.id": "unread-1", "gen_ai.conversation.id": "unread-1",
                              "gen_ai.operation.name": "chat", "gen_ai.provider.name": "anthropic",
                              "gen_ai.request.model": "claude-sonnet-5-5",
                              # As text, the way instrumentations record it: one
                              # value, 92 bytes inline, then 303 offloaded.
                              "gen_ai.input.messages": json.dumps(messages, separators=(",", ":"))}))
    post(spans)
    save("xray_unread.json", "/v1/xray/" + trace + "/00000000000000d1", {"resolve": "false"})


def bucket(n):
    return "" if n == 1 else "×2" if n == 2 else "×3-5" if n <= 5 else "×6+"


def path_to_tql(path, root):
    """The query the dashboard's Find these runs sends (ui/src/lib/insight.js
    pathToTql); None where it offers no search."""
    atoms, node, last = [], root, None
    for token in path:
        children = (node or {}).get("children") or []
        if sum(1 for c in children if c["token"] == token) > 1:
            return None
        node = next((c for c in children if c["token"] == token), None)
        kind, _, name = token.partition(":")
        if kind not in ("llm", "tool", "agent"):
            return None
        mark = re.search(r"×(2|3-5|6\+)$", name)
        bare = name[:-len(mark.group(0))] if mark else name
        if not bare or bare.endswith("…") or "×" in bare or re.search(r"[\x00-\x1f]", bare):
            return None
        signature = json.dumps(f"{kind}:{bare}", ensure_ascii=False)
        count = "" if not mark else {"2": "{2}", "3-5": "{3,5}", "6+": "{6,}"}[mark.group(1)]
        atoms.append(f"{kind}[signature={signature}]{count}")
        last = (signature, bool(mark) and mark.group(1) == "6+")
    head = "^" + " -> ".join(atoms)
    return head if last[1] else f"{head}$ | {head} -> step[signature!={last[0]}]"


def wave3(path):
    """The query the wave-3 dashboard sent: names, not signatures, and no end."""
    atoms = []
    for token in path:
        kind, _, name = token.partition(":")
        mark = re.search(r"×(2|3-5|6\+)$", name)
        bare = name[:-len(mark.group(0))] if mark else name
        count = "" if not mark else {"2": "{2}", "3-5": "{3,5}", "6+": "{6,}"}[mark.group(1)]
        atoms.append(f"{kind}({json.dumps(bare, ensure_ascii=False)}){count}")
    return "^" + " -> ".join(atoms)


def loops():
    """Ten completed runs, three hours old."""
    start = NOW - 3 * 3600 * SEC
    spans = []
    plans = [(f"s{n}", "loop-agent", [("llm", "gpt-6.1-sol")] + [("tool", "search")] * n,
              "success" if n % 2 else "failure") for n in range(1, 8)]
    plans.append(("cap3", "loop-agent", [("llm", "gpt-6.1-sol")] + [("tool", "Search")] * 3, "success"))
    plans.append(("lit", "mini-agent", [("llm", "gpt-6-luna"), ("tool", "search×2"), ("llm", "gpt-6-luna")],
                  "success"))
    plans.append(("two", "mini-agent", [("llm", "gpt-6-luna"), ("tool", "search"), ("tool", "search"),
                                        ("llm", "gpt-6-luna")], "failure"))
    for i, (run, agent, steps, outcome) in enumerate(plans):
        spans += steps_run(format(0x100B5000 + i, "032x"), run, agent, start + i * 30 * SEC, steps, outcome)
    post(spans)
    loop_map = save("fate_map_loops.json", "/v1/fate/map", {"agent": "loop-agent", "min_support": 1})
    save("fate_map_mini.json", "/v1/fate/map", {"agent": "mini-agent", "min_support": 1})
    answers = {}
    def walk(node, path):
        for child in node.get("children") or []:
            here = path + [child["token"]]
            for q in (path_to_tql(here, loop_map["root"]), wave3(here)):
                if q is not None and q not in answers:
                    status, body = get("/v1/trajectories/search", {"q": q, "agent": "loop-agent", "limit": 100})
                    assert status == 200, (q, status, body)
                    answers[q] = json.loads(body)
            walk(child, here)
    walk(loop_map["root"], [])
    with open(f"{OUT}/tql_loops.json", "w", encoding="utf-8") as f:
        json.dump(answers, f, ensure_ascii=False, separators=(",", ":"))
    print(f"200 tql_loops.json ({len(answers)} queries)")


def tenants():
    """support-agent in acme and globex, 1.0.0 then 1.1.0, a run every 20 minutes."""
    spans = []
    for t, tenant in enumerate(("acme", "globex")):
        for n in range(60):
            version = "1.1.0" if n >= 30 else "1.0.0"
            failed = n % 15 == 7 if (tenant == "acme" or n < 30) else n % 3 == 1
            steps = [("llm", "gpt-6.1-sol"), ("tool", "lookup_order"), ("llm", "gpt-6.1-sol")]
            spans += steps_run(format(0x7E4A0000 + t * 0x100 + n, "032x"), f"{tenant}-{n}", "support-agent",
                               NOW - 30 * 3600 * SEC + n * 20 * MIN + t * 7 * SEC, steps,
                               "failure" if failed else "success", extra={"service.version": version},
                               tenant=tenant)
    post(spans)
    save("changes_tenants.json", "/v1/changes")


def removed():
    """One live run, a second that takes the guard's one slot, then quiet."""
    def run_span(run, n):
        t = time.time_ns()
        return [span(format(0x5001 + n, "032x"), "0000000000000001", "chat sim-small", "support-app", t, 5 * MS,
                     {"session.id": run, "gen_ai.operation.name": "chat", "gen_ai.request.model": "sim-small"})]
    post(run_span("tkt-5001", 0))
    received = bytearray()
    resp = urllib.request.urlopen(BASE + "/v1/runs/live/stream", timeout=30)
    def read():
        try:
            while True:
                chunk = resp.read1(65536)
                if not chunk:
                    return
                received.extend(chunk)
        except Exception:
            return
    reader = threading.Thread(target=read, daemon=True)
    reader.start()
    while b"event: snapshot" not in received:
        time.sleep(0.05)
    post(run_span("tkt-5002", 1))
    deadline = time.time() + 15
    while received.count(b"event: removed") < 2 and time.time() < deadline:
        time.sleep(0.1)
    resp.close()
    with open(f"{OUT}/live_stream_removed.txt", "wb") as f:
        f.write(bytes(received))
    print(f"200 live_stream_removed.txt ({received.count(b'event: ')} frames)")


{"blame": blame, "params": params, "long": long_context, "unread": unread, "loops": loops,
 "tenants": tenants, "removed": removed}[STORE]()
