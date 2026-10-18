#!/usr/bin/env python3
"""Simulated agents for the Mission Control demo. Standard library only.

  agents.py history URL
      Posts the completed runs the Fate Map learns from: 180 support-agent
      runs down four paths with known outcomes, finished hours ago.

  agents.py live URL [--seconds N] [--concurrency N] [--tempo X] [--verify]
      Runs agents in real time. Each run is a thread that behaves like an
      instrumented agent: before every step it asks the run guard
      (POST /v1/fuses/check, naming the step), hears nudges there, holds on a
      stop until someone resets it (abandoning after a while), and posts each
      step's span when the step ends — wall-clock timestamps, the messages it
      sent and got, tool arguments and results, metered cost.

      --verify checks, against the server, every claim the Live screen and
      the Run screen are built on, and exits non-zero if one fails.

Every number printed is read back from the server, or is labelled as the
generator's own tally.
"""

import argparse
import json
import random
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

NS = 1_000_000_000
LOCK = threading.Lock()
RNG = random.Random(20261018)


def say(*parts):
    with LOCK:
        print(*parts, flush=True)


def warn(*parts):
    """What went wrong, on stderr: CI keeps stderr when it throws stdout away."""
    with LOCK:
        print(*parts, file=sys.stderr, flush=True)


# ------------------------------------------------------------------ HTTP


class RedirectRefused(Exception):
    """A 3xx answered a request that may have carried a token."""


class RefuseRedirects(urllib.request.HTTPRedirectHandler):
    """Refuses every redirect: urllib would copy the Authorization header to
    wherever a redirect pointed. Traza never redirects its own routes."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise RedirectRefused("%s answered %d, a redirect to %s: not followed" % (req.full_url, code, newurl))


OPENER = urllib.request.build_opener(RefuseRedirects)


class Api:
    def __init__(self, base, token=None):
        self.base = base.rstrip("/")
        self.token = token

    def call(self, method, path, body=None, timeout=15):
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(self.base + path, data=data, method=method)
        if data is not None:
            req.add_header("Content-Type", "application/json")
        if self.token:
            req.add_header("Authorization", "Bearer " + self.token)
        try:
            with OPENER.open(req, timeout=timeout) as reply:
                raw = reply.read()
                return reply.status, (json.loads(raw) if raw else None)
        except urllib.error.HTTPError as error:
            raw = error.read()
            try:
                return error.code, json.loads(raw)
            except ValueError:
                return error.code, {"error": raw.decode("utf-8", "replace")}

    def get(self, path):
        return self.call("GET", path)

    def post(self, path, body):
        return self.call("POST", path, body)

    def stream(self, path, seconds):
        """Reads a server-sent-events stream for up to `seconds`: the frames."""
        req = urllib.request.Request(self.base + path)
        if self.token:
            req.add_header("Authorization", "Bearer " + self.token)
        frames, buffered = [], b""
        deadline = time.time() + seconds
        with OPENER.open(req, timeout=seconds) as reply:
            while time.time() < deadline:
                chunk = reply.read1(65536) if hasattr(reply, "read1") else reply.read(4096)
                if not chunk:
                    break
                buffered += chunk
                while b"\n\n" in buffered:
                    frame, buffered = buffered.split(b"\n\n", 1)
                    frames.append(frame.decode())
                if any(f.startswith("event: run") for f in frames):
                    break
        return frames


def q(run):
    return urllib.parse.quote(run, safe="")


def hexid(n):
    return uuid.uuid4().hex[:n]


# ------------------------------------------------------------- the work

TICKETS = [
    "My order #A-1043 arrived broken, I want my money back.",
    "I was charged twice for order #B-2210.",
    "Where is my refund for #C-7781? It has been two weeks.",
    "Order #D-5520 never arrived. Please refund it.",
    "Can you cancel and refund #E-9904? Wrong size.",
    "Your app double-billed me for order #F-3317.",
]
SYSTEM = "You are a support agent. Use tools to find orders and issue refunds. Be brief."
GPT = ("llm", "gpt-4o")

# The support agent's paths. Their names and steps are the history's, so
# foresight has something to judge a live run against.
PATHS = {
    "refund": [GPT, ("tool", "search_orders"), GPT, ("tool", "issue_refund"), GPT],
    "search_loop": [GPT] + [("tool", "search_orders")] * 6 + [GPT],
    "escalate": [GPT, ("tool", "lookup_customer"), GPT, ("tool", "escalate_ticket"), GPT],
    "double_refund": [GPT, ("tool", "search_orders"), GPT, ("tool", "issue_refund"), ("tool", "issue_refund"), GPT],
    "novel": [GPT, ("tool", "lookup_customer"), GPT, ("tool", "export_customer_data"), GPT],
}
WEIGHTS = [("refund", 5), ("search_loop", 4), ("escalate", 2), ("double_refund", 2), ("novel", 1)]
# What a looping agent does once told to move on.
RECOVER = [GPT, ("tool", "issue_refund"), GPT]
# How the history's runs ended: (runs, successes, abandoned) per path.
HISTORY = {"refund": (60, 54, 0), "search_loop": (50, 5, 5), "escalate": (40, 24, 4), "double_refund": (30, 15, 0)}
HISTORY_PATHS = {
    "refund": PATHS["refund"],
    "search_loop": [GPT] + [("tool", "search_orders")] * 4 + [GPT],
    "escalate": PATHS["escalate"],
    "double_refund": PATHS["double_refund"],
}


def order_of(ticket):
    return ticket.split("#")[1].split()[0].rstrip("?.,")


def tool_result(name, attempt, ticket):
    order = order_of(ticket)
    if name == "search_orders":
        if attempt > 0:
            return {"orders": [], "query": order, "note": "no match"}
        return {"orders": [{"id": order, "total_usd": 84.0, "status": "delivered"}]}
    if name == "issue_refund":
        return {"refund_id": "rf_" + hexid(8), "order": order, "amount_usd": 84.0, "status": "issued"}
    if name == "lookup_customer":
        return {"customer": "cus_" + hexid(6), "tier": "gold", "open_tickets": 2}
    if name == "escalate_ticket":
        return {"ticket": "esc_" + hexid(6), "queue": "billing-humans"}
    if name == "lookup_invoice":
        return {"invoice": "in_" + hexid(6), "lines": 3, "disputed": True}
    return {"rows": 1240, "destination": "s3://exports/customers.csv"}


def span(trace, parent, name, service, run, t0, t1, status, attrs, span_id=None):
    return {
        "trace_id": trace, "span_id": span_id or hexid(16), "parent_span_id": parent, "name": name,
        "service": service, "status": status, "start_time_ns": int(t0 * NS), "end_time_ns": int(t1 * NS),
        "attributes": {"session.id": run, **attrs}, "events": [],
    }


# --------------------------------------------------------------- history


def post_history(api):
    """The completed runs the Fate Map learns from, ended two hours ago and
    earlier, each with its outcome declared."""
    spans, serial = [], 0
    end = time.time() - 2 * 3600
    for path, (runs, successes, abandoned) in HISTORY.items():
        for i in range(runs):
            if i < successes:
                outcome = "success"
            elif i < successes + abandoned:
                outcome = "abandoned"
            else:
                outcome = "failure"
            trace, root, run = hexid(32), hexid(16), f"hist-{path}-{i}"
            clock = end - serial * 45
            start = clock
            for position, (kind, name) in enumerate(HISTORY_PATHS[path]):
                t1 = clock + RNG.uniform(0.3, 1.2)
                if kind == "llm":
                    attrs = {"gen_ai.operation.name": "chat", "gen_ai.request.model": name,
                             "gen_ai.usage.input_tokens": 600 + 40 * position, "gen_ai.usage.output_tokens": 90,
                             "llm.cost_usd": 0.0024}
                    status = "ok"
                else:
                    attrs = {"gen_ai.operation.name": "execute_tool", "gen_ai.tool.name": name,
                             "gen_ai.tool.call.arguments": json.dumps({"attempt": position})}
                    status = "error" if path == "search_loop" and name == "search_orders" and position < 4 else "ok"
                spans.append(span(trace, root, f"{'chat' if kind == 'llm' else 'execute_tool'} {name}",
                                  "support-agent", run, clock, t1, status, attrs))
                clock = t1 + 0.05
            spans.append(span(trace, None, "support.handle_ticket", "support-agent", run, start, clock,
                              "error" if outcome == "failure" else "ok",
                              {"session.outcome": outcome, "traceloop.span.kind": "workflow"}, span_id=root))
            serial += 1
    for i in range(0, len(spans), 500):
        status, ack = api.post("/v1/spans", {"spans": spans[i:i + 500]})
        if status != 200:
            raise SystemExit(f"history: ingest answered {status}: {ack}")
    say(f"history: {serial} completed support-agent runs posted ({len(spans)} spans)")


# ------------------------------------------------------------- live runs


class Run(threading.Thread):
    """One support-agent run, asking the guard before every step."""

    service = "support-agent"

    def __init__(self, api, run_id, path, tempo, hold_seconds):
        super().__init__(daemon=True)
        self.api, self.run_id, self.path, self.tempo, self.hold_seconds = api, run_id, path, tempo, hold_seconds
        self.steps = list(PATHS.get(path, []))
        self.trace, self.root = hexid(32), hexid(16)
        self.ticket = RNG.choice(TICKETS)
        self.messages = [{"role": "system", "content": SYSTEM}, {"role": "user", "content": self.ticket}]
        self.started = time.time()
        self.position = 0
        self.searches = 0
        self.outcome = None
        self.heard = []        # nudges delivered by a check: [{id, kind, message}]
        self.stops = []        # reasons a check answered stop with, per hold
        self.holding = False
        self.released = 0
        self.took_advice = False

    def pause(self, low, high):
        time.sleep(RNG.uniform(low, high) / self.tempo)

    def check(self, kind, name):
        nxt = {"tool": name} if kind == "tool" else {"est_cost_usd": 0.004}
        status, verdict = self.api.post("/v1/fuses/check", {"run": self.run_id, "next": nxt})
        if status != 200:
            warn(f"  {self.run_id}: the check answered {status}: {verdict}")
            return "continue", [], []
        nudges = verdict.get("nudges") or []
        for nudge in nudges:
            self.heard.append(nudge)
            say(f"  {self.run_id}: heard a {nudge['kind']} nudge {nudge['id']}: {nudge['message']!r}")
        return verdict.get("verdict", "continue"), verdict.get("reasons") or [], nudges

    def hold(self, reasons):
        """Stopped: wait for someone to reset the run. A verdict read here is
        neither activity nor a pre-call check, so it takes no nudges."""
        self.stops.append(reasons)
        self.holding = True
        say(f"  {self.run_id}: stopped before step {self.position}: {'; '.join(reasons)[:110]}")
        deadline = time.time() + self.hold_seconds
        try:
            while time.time() < deadline:
                time.sleep(1.0)
                status, doc = self.api.get(f"/v1/fuses/runs/{q(self.run_id)}")
                if status == 200 and doc.get("verdict") != "stop":
                    self.released += 1
                    say(f"  {self.run_id}: released ({doc.get('verdict')}), carrying on")
                    return True
            say(f"  {self.run_id}: nobody released it; abandoning")
            return False
        finally:
            self.holding = False

    def llm(self, model, final, next_tool, agent=None, parent=None):
        t0 = time.time()
        self.pause(1.2, 3.2)
        tokens_in = 380 + 70 * self.position + RNG.randint(0, 40)
        tokens_out = RNG.randint(40, 180)
        if next_tool:
            out = {"role": "assistant", "content": None, "tool_calls": [
                {"id": "call_" + hexid(8), "name": next_tool, "arguments": {"ticket": self.ticket[:40]}}]}
        elif final:
            out = {"role": "assistant", "content": {
                "refund": "I've issued a full refund of $84.00. It will reach your card in 3-5 days.",
                "escalate": "I've passed this to our billing team; they will reply within a day.",
            }.get(self.path, "I've sorted this out for you. Anything else?")}
        else:
            out = {"role": "assistant", "content": "Let me look into that."}
        attrs = {"gen_ai.operation.name": "chat", "gen_ai.request.model": model,
                 "gen_ai.usage.input_tokens": tokens_in, "gen_ai.usage.output_tokens": tokens_out,
                 "llm.cost_usd": round(tokens_in * 2.5e-6 + tokens_out * 1e-5, 6),
                 "gen_ai.input.messages": json.dumps(self.messages), "gen_ai.output.messages": json.dumps([out])}
        if agent:
            attrs["gen_ai.agent.name"] = agent
        self.messages.append({k: v for k, v in out.items() if v is not None})
        return span(self.trace, parent or self.root, f"chat {model}", self.service, self.run_id, t0, time.time(), "ok", attrs)

    def tool(self, name, agent=None, parent=None):
        t0 = time.time()
        self.pause(0.5, 1.4) if name == "search_orders" else self.pause(0.6, 2.0)
        attempt = self.searches if name == "search_orders" else 0
        if name == "search_orders":
            self.searches += 1
        result = tool_result(name, attempt, self.ticket)
        self.messages.append({"role": "tool", "content": json.dumps(result)})
        attrs = {"gen_ai.operation.name": "execute_tool", "gen_ai.tool.name": name,
                 "gen_ai.tool.call.arguments": json.dumps({"ticket": self.ticket[:40], "attempt": attempt}),
                 "gen_ai.tool.call.result": json.dumps(result)}
        if agent:
            attrs["gen_ai.agent.name"] = agent
        failed = name == "search_orders" and attempt > 0
        return span(self.trace, parent or self.root, f"execute_tool {name}", self.service, self.run_id, t0,
                    time.time(), "error" if failed else "ok", attrs)

    def run(self):
        try:
            self.work()
        except Exception as error:  # a demo agent that dies says why
            warn(f"  {self.run_id}: crashed: {error!r}")
            self.outcome = "crashed"

    def work(self):
        while self.position < len(self.steps):
            kind, name = self.steps[self.position]
            verdict, reasons, nudges = self.check(kind, name)
            if any(n["kind"] == "advice" for n in nudges) and self.path in ("search_loop", "double_refund", "novel"):
                say(f"  {self.run_id}: taking the advice; moving on to the refund")
                self.steps = self.steps[: self.position] + RECOVER
                self.path = "refund"
                self.took_advice = True
                continue
            if any(n["kind"] == "block" for n in nudges):
                say(f"  {self.run_id}: step {self.position} ({kind}:{name}) vetoed by a person; skipping it")
                self.steps.pop(self.position)
                time.sleep(2.2)  # the veto holds two seconds
                continue
            if verdict == "stop":
                if not self.hold(reasons):
                    return self.finish("abandoned")
                continue
            self.pause(0.2, 0.5)
            if kind == "llm":
                nxt = self.steps[self.position + 1] if self.position + 1 < len(self.steps) else None
                record = self.llm(name, nxt is None, nxt[1] if nxt and nxt[0] == "tool" else None)
            else:
                record = self.tool(name)
            self.api.post("/v1/spans", {"spans": [record]})
            self.position += 1
            self.pause(0.5, 2.0)
        odds = {"refund": 0.92, "escalate": 0.65, "double_refund": 0.5, "search_loop": 0.1, "novel": 0.3}
        self.finish("success" if RNG.random() < odds.get(self.path, 0.5) else "failure")

    def finish(self, outcome, root_attrs=None):
        self.outcome = outcome
        attrs = root_attrs if root_attrs is not None else {"traceloop.span.kind": "workflow"}
        self.api.post("/v1/spans", {"spans": [span(
            self.trace, None, "support.handle_ticket", self.service, self.run_id, self.started, time.time(),
            "error" if outcome == "failure" else "ok", {"session.outcome": outcome, **attrs}, span_id=self.root)]})
        say(f"  {self.run_id}: {outcome} after {self.position} steps")


class TeamRun(Run):
    """A support-app run: named agents handing a ticket around. In a
    ping-pong, triage and billing hand it back and forth."""

    service = "support-app"

    def __init__(self, api, run_id, path, tempo, hold_seconds):
        super().__init__(api, run_id, path, tempo, hold_seconds)
        self.messages = [{"role": "system", "content": "Route billing tickets."}, {"role": "user", "content": self.ticket}]
        self.turns = ["triage", "billing"] * 6 if path == "ping_pong" else ["triage", "billing", "refunds"]

    def work(self):
        turn = 0
        while turn < len(self.turns):
            agent = self.turns[turn]
            work = [("llm", "claude-sonnet-4-5")]
            if agent == "billing":
                work.append(("tool", "lookup_invoice"))
            if agent == "refunds":
                work += [("tool", "issue_refund"), ("llm", "claude-sonnet-4-5")]
            agent_id, a0 = hexid(16), time.time()
            for kind, name in work:
                while True:
                    verdict, reasons, nudges = self.check(kind, name)
                    if any(n["kind"] == "advice" for n in nudges) and self.path == "ping_pong":
                        say(f"  {self.run_id}: taking the advice; handing the ticket to refunds")
                        self.turns = self.turns[: turn + 1] + ["refunds"]
                        self.took_advice = True
                    if verdict != "stop":
                        break
                    if not self.hold(reasons):
                        self.post_agent(agent, agent_id, a0)
                        return self.finish("abandoned", {})
                self.pause(0.2, 0.4)
                record = self.llm(name, False, None, agent, agent_id) if kind == "llm" else self.tool(name, agent, agent_id)
                self.api.post("/v1/spans", {"spans": [record]})
                self.position += 1
                self.pause(0.3, 1.0)
            self.post_agent(agent, agent_id, a0)
            turn += 1
        self.finish("success" if self.path != "ping_pong" else "failure", {})

    def post_agent(self, agent, agent_id, a0):
        self.api.post("/v1/spans", {"spans": [span(
            self.trace, self.root, f"invoke_agent {agent}", self.service, self.run_id, a0, time.time(), "ok",
            {"gen_ai.operation.name": "invoke_agent", "gen_ai.agent.name": agent}, span_id=agent_id)]})


POLICIES = [
    {"name": "support", "match": {"service": "support-agent"},
     "limits": {"min_success_odds": 0.3, "max_cost_usd": 0.25}, "action": "stop"},
    {"name": "support-app", "match": {"service": "support-app"},
     "limits": {"max_handoff_cycles": 3, "max_cost_usd": 0.5}, "action": "stop"},
]


class Fleet:
    """Keeps `concurrency` runs going, the first few chosen so every beat
    has a run to look at early."""

    OPENING = ["search_loop", "ping_pong", "refund", "search_loop", "refund", "escalate"]

    def __init__(self, api, concurrency, tempo, hold_seconds):
        self.api, self.concurrency, self.tempo, self.hold_seconds = api, concurrency, tempo, hold_seconds
        self.runs, self.all, self.serial = [], [], 4100
        self.stopped = threading.Event()

    def spawn(self):
        self.serial += 1
        path = self.OPENING.pop(0) if self.OPENING else None
        if path is None:
            if RNG.random() < 0.2:
                path = "ping_pong" if RNG.random() < 0.6 else "handoff"
            else:
                names, weights = zip(*WEIGHTS)
                path = RNG.choices(names, weights)[0]
        if path in ("ping_pong", "handoff"):
            run = TeamRun(self.api, f"conv-{self.serial}", path, self.tempo, self.hold_seconds)
        else:
            run = Run(self.api, f"tkt-{self.serial}", path, self.tempo, self.hold_seconds)
        run.start()
        self.runs.append(run)
        self.all.append(run)

    def keep_going(self):
        while not self.stopped.is_set():
            self.runs = [r for r in self.runs if r.is_alive()]
            while len(self.runs) < self.concurrency and not self.stopped.is_set():
                self.spawn()
                time.sleep(RNG.uniform(0.2, 0.8) / self.tempo)
            time.sleep(0.3)

    def find(self, predicate, seconds, what):
        deadline = time.time() + seconds
        while time.time() < deadline:
            for run in list(self.all):
                if predicate(run):
                    return run
            time.sleep(0.25)
        raise Failed(f"no run {what} within {seconds:g} s")


# ------------------------------------------------------------- the beats


class Failed(Exception):
    pass


def until(probe, seconds, what):
    deadline = time.time() + seconds
    last = None
    while time.time() < deadline:
        last = probe()
        if last:
            return last
        time.sleep(0.4)
    raise Failed(f"{what} within {seconds:g} s")


def claim(ok, what):
    if not ok:
        raise Failed(what)
    say(f"  ✓ {what}")


def verify(api, fleet):
    say("\n1. Foresight on the live view")
    def foresight():
        status, doc = api.get("/v1/runs/live")
        rows = doc.get("runs", []) if status == 200 else []
        verdicts = {}
        for row in rows:
            verdict = (row.get("foresight") or {}).get("verdict")
            if verdict:
                verdicts.setdefault(verdict, row)
        return verdicts if "likely_failure" in verdicts and "on_track" in verdicts else None
    verdicts = until(foresight, 40, "a likely failure and an on-track run listed with foresight")
    doomed = verdicts["likely_failure"]["foresight"]
    claim(doomed["ci95"][1] < 0.5, f"{verdicts['likely_failure']['run']} is a likely failure: {round(doomed['p_success'] * 100)}% "
          f"of {doomed['support']} earlier runs on its path succeeded (95% CI upper bound {round(doomed['ci95'][1] * 100)}%, "
          f"against a base rate of {round(doomed['base_rate'] * 100)}%)")
    claim(verdicts["on_track"]["foresight"]["verdict"] == "on_track", f"{verdicts['on_track']['run']} is on track")

    say("\n2. The stream")
    frames = api.stream("/v1/runs/live/stream", 15)
    claim(frames and frames[0].startswith("event: snapshot"), "GET /v1/runs/live/stream opens with a snapshot")
    claim(any(f.startswith("event: run") for f in frames), "and sends a row as a run changes")

    say("\n3. A run held by its history")
    held = fleet.find(lambda r: any(rs and rs[0].startswith("foresight:") for rs in r.stops) and r.holding, 60,
                      "held by a foresight stop")
    # What the check itself answered, naming the step about to happen — not
    # the verdict read, which judges the path so far without it.
    answered = next(rs for rs in reversed(held.stops) if rs and rs[0].startswith("foresight:"))[0]
    claim(answered.startswith("foresight:"), f"{held.run_id}'s pre-call check, naming its next step, answered stop: {answered}")
    status, doc = api.get(f"/v1/fuses/runs/{q(held.run_id)}")
    if status == 200 and doc.get("verdict") == "stop" and (doc.get("foresight") or {}).get("raised"):
        claim(True, f"its verdict read, which judges the path so far without that step, says stop too "
              f"({doc['reasons'][0]}), so the Live screen marks it held")
    else:
        say(f"  its verdict read, which judges the path so far without that step, says "
            f"{doc.get('verdict') if status == 200 else status}: the Live screen shows it waiting, not held")
    _, live = api.get("/v1/runs/live")
    row = next((r for r in live["runs"] if r["run"] == held.run_id), None)
    claim(row is not None and row["verdict"] != "stop" and row["current"]["pending"],
          "the live listing never carries a foresight stop: it shows the step pending")

    say("\n4. A nudge, and an approval")
    status, nudge = api.post(f"/v1/runs/{q(held.run_id)}/nudges", {
        "message": "The order search keeps coming back empty. Stop searching and issue the refund.", "kind": "advice"})
    claim(status == 201 and nudge["status"] == "pending", f"nudge {nudge.get('id')} queued, pending while the run holds")
    status, doc = api.post(f"/v1/fuses/runs/{q(held.run_id)}/reset", {"grant": {"min_success_odds": 0.05}})
    claim(status == 200 and doc["verdict"] != "stop", "approved to continue with a grant of lower odds (min_success_odds 0.05)")
    until(lambda: held.took_advice, 15, f"{held.run_id} hearing the nudge and taking it")
    claim(any(n["id"] == nudge["id"] for n in held.heard), f"{held.run_id} heard {nudge['id']} at its next check, and moved on to the refund")
    _, listed = api.get(f"/v1/runs/{q(held.run_id)}/nudges")
    mine = next(n for n in listed["nudges"] if n["id"] == nudge["id"])
    claim(mine["status"] == "delivered",
          f"the nudge lists as delivered, {(mine['delivered_at_ns'] - mine['created_at_ns']) / 1e9:.1f} s after it was queued")

    say("\n5. The kill switch")
    # A run with steps left to take: one on its last step may finish without
    # asking again, and then there is no next check to refuse. If one does
    # anyway, another is tripped.
    tried = set()
    for _ in range(3):
        target = fleet.find(lambda r: r.is_alive() and not r.holding and not r.stops and r.run_id not in tried
                            and 1 <= r.position <= len(r.steps) - 3 and r.path in ("refund", "escalate")
                            and not isinstance(r, TeamRun), 30, "running freely down a healthy path")
        tried.add(target.run_id)
        status, doc = api.post(f"/v1/fuses/runs/{q(target.run_id)}/trip", {"reason": "taking over by hand"})
        claim(status == 200 and doc["verdict"] == "stop" and doc["tripped_by"] == "manual", f"{target.run_id} tripped by hand")
        held = until(lambda: any(rs and "manually tripped" in rs[0] for rs in target.stops) or (not target.is_alive() and "done"),
                     20, f"{target.run_id} stopping at its next check")
        if held is True:
            break
        say(f"  {target.run_id} finished without asking again; tripping another")
        api.post(f"/v1/fuses/runs/{q(target.run_id)}/reset", {})
    else:
        raise Failed("three runs in a row finished before their next check")
    claim(True, f"{target.run_id}'s next pre-call check answered stop, and it holds")
    api.post(f"/v1/fuses/runs/{q(target.run_id)}/reset", {})
    until(lambda: target.released, 10, f"{target.run_id} carrying on after the reset")
    claim(True, f"{target.run_id} carried on once reset")

    say("\n6. Agents bouncing a ticket")
    pong = fleet.find(lambda r: isinstance(r, TeamRun) and any(rs and rs[0].startswith("max_handoff_cycles") for rs in r.stops),
                      90, "stopped for handing a ticket back and forth")
    _, top = api.get(f"/v1/runs/{q(pong.run_id)}/topology")
    cycles = {tuple(c["agents"]): c["count"] for c in top["cycles"]}
    claim(max(cycles.values() or [0]) >= 3, f"{pong.run_id}: {pong.stops[0][0]}; the topology counts "
          + ", ".join(f"{a} → {b} → {a} ×{n}" for (a, b), n in cycles.items()))

    say("\n7. A fork")
    parent = fleet.find(lambda r: not r.is_alive() and r.outcome == "success" and r.path == "refund"
                        and not r.took_advice and not isinstance(r, TeamRun), 90, "finished down the refund path")
    status, replay = api.post("/v1/replays", {"run": parent.run_id, "tenant": "",
                                              "edits": [{"step": 1, "tool_result": {"orders": [], "note": "no match"}}],
                                              "live_after": 1})
    claim(status == 201 and replay["fork"]["step"] == 1, f"forked {parent.run_id} at step 1: {replay.get('fork_id')}")
    run_fork(api, parent, replay)
    def listed():
        _, doc = api.get(f"/v1/forks?run={q(parent.run_id)}&tenant=")
        return next((f for f in doc.get("forks", []) if f["fork_id"] == replay["fork_id"] and f["diffed"]), None)
    fork = until(listed, 15, "the fork listed, compared with its parent")
    claim(fork["first_divergence"]["b"] == 1, f"GET /v1/forks lists it, {fork['outcome']}: {fork['first_divergence']['explanation']}")


def run_fork(api, parent, replay):
    """The fork's harness, by hand: model calls to the replay until it answers
    replay_live, tool results from tools/next; past the cut, a stand-in
    provider answers. Spans go under the fork's own session id, tagged."""
    fork, rid = replay["fork"], replay["id"]
    tags = dict(fork["attributes"])
    run_id = fork["id"]
    trace, root, started = hexid(32), hexid(16), time.time()
    messages = [{"role": "system", "content": SYSTEM}, {"role": "user", "content": parent.ticket}]
    spans, live = [], False
    for kind, name in PATHS["refund"]:
        t0 = time.time()
        if kind == "llm":
            answer = None
            if not live:
                status, body = api.post(f"/v1/replays/{rid}/openai/v1/chat/completions", {"model": name, "messages": messages})
                if status == 409 and body["error"]["type"] == "replay_live":
                    live = True
                else:
                    if status != 200:
                        raise Failed(f"the replay refused the fork's model call: {status} {body}")
                    msg = body["choices"][0]["message"]
                    answer = {"role": "assistant", "content": msg.get("content")}
                    if msg.get("tool_calls"):
                        answer["tool_calls"] = [{"id": c["id"], "name": c["function"]["name"],
                                                 "arguments": json.loads(c["function"]["arguments"])} for c in msg["tool_calls"]]
            if answer is None:
                answer = {"role": "assistant", "content": "I could not find that order; I've asked a person to check it."}
            spans.append(span(trace, root, f"chat {name}", "support-agent", run_id, t0, time.time(), "ok", {
                **tags, "gen_ai.operation.name": "chat", "gen_ai.request.model": name,
                "gen_ai.usage.input_tokens": 500, "gen_ai.usage.output_tokens": 40, "llm.cost_usd": 0.00165,
                "gen_ai.input.messages": json.dumps(messages), "gen_ai.output.messages": json.dumps([answer])}))
            messages.append({k: v for k, v in answer.items() if v is not None})
            if live:
                break
        else:
            status, body = api.post(f"/v1/replays/{rid}/tools/next", {"tool": name})
            if status != 200:
                raise Failed(f"tools/next answered {status}: {body}")
            spans.append(span(trace, root, f"execute_tool {name}", "support-agent", run_id, t0, time.time(), "ok", {
                **tags, "gen_ai.operation.name": "execute_tool", "gen_ai.tool.name": name,
                "gen_ai.tool.call.arguments": json.dumps(body.get("arguments")),
                "gen_ai.tool.call.result": json.dumps(body["result"])}))
            messages.append({"role": "tool", "content": json.dumps(body["result"])})
    spans.append(span(trace, None, "support.handle_ticket", "support-agent", run_id, started, time.time(), "error",
                      {**tags, "session.outcome": "failure"}, span_id=root))
    status, _ = api.post("/v1/spans", {"spans": spans})
    if status != 200:
        raise Failed(f"the fork's spans were refused: {status}")


# ------------------------------------------------------------------ main


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("mode", choices=["history", "live"])
    parser.add_argument("url")
    parser.add_argument("--token", help="a bearer token (default: none, an open store)")
    parser.add_argument("--seconds", type=float, default=0, help="how long to run; 0 runs until Ctrl-C")
    parser.add_argument("--concurrency", type=int, default=8)
    parser.add_argument("--tempo", type=float, default=1.0, help="a multiplier on the agents' pace")
    parser.add_argument("--hold", type=float, default=90, help="seconds a stopped run waits to be released")
    parser.add_argument("--verify", action="store_true")
    args = parser.parse_args()
    api = Api(args.url, args.token)
    if args.mode == "history":
        post_history(api)
        return 0

    for policy in POLICIES:
        status, body = api.post("/v1/fuses/policies", policy)
        if status != 200:
            raise SystemExit(f"policy {policy['name']}: {status} {body}")
    say("policies: support-agent stops a run whose path's odds fall below 0.3; support-app stops at 3 handoff cycles")
    fleet = Fleet(api, args.concurrency, max(0.1, args.tempo), args.hold)
    driver = threading.Thread(target=fleet.keep_going, daemon=True)
    driver.start()
    end = time.time() + args.seconds if args.seconds else None
    code = 0
    try:
        if args.verify:
            try:
                verify(api, fleet)
                say("\nverified: every claim above was read back from the server")
            except (Failed, RedirectRefused, OSError) as error:
                warn(f"\nFAILED: {error}")
                code = 1
                end = time.time()
        while code == 0 and (end is None or time.time() < end):
            time.sleep(0.5)
    except KeyboardInterrupt:
        pass
    fleet.stopped.set()
    runs = fleet.all

    def n(count, word):
        return f"{count} {word}" + ("" if count == 1 else "s")
    say(f"\ngenerator's tally: {n(len(runs), 'run')} started, "
        f"{sum(1 for r in runs if r.outcome == 'success')} succeeded, "
        f"{sum(1 for r in runs if r.outcome == 'abandoned')} abandoned after a stop nobody released, "
        f"{n(sum(len(r.heard) for r in runs), 'nudge')} heard, {n(sum(len(r.stops) for r in runs), 'stop')} held")
    return code


if __name__ == "__main__":
    sys.exit(main())
