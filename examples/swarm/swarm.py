#!/usr/bin/env python3
"""The generator behind examples/swarm: three simulated agent services
streaming spans into a live Traza server, in real time.

stdlib only. Every span carries wall-clock nanosecond timestamps and is
POSTed in the batch tick after its end time passes, so the live tail and
the 15-minute overview window behave exactly as they would under a real
platform: planners land before their workers, workers before the reduce.

Two kinds of numbers print, and each is labeled as what it is. The
server-read figures — ack counts summed from /v1/spans responses, costs
from /v1/stats/llm, sessions from /v1/sessions, everything in the
verification gate — are read back from the live server, never echoed
from what was sent. The generator's own tallies (sessions started,
fan-outs, error spans emitted, spans still in flight) are its emission
counters: honest bookkeeping, not server measurements.
"""

import heapq
import json
import os
import random
import signal
import sys
import time
import urllib.error
import urllib.request

NS = 1_000_000_000
BOLD, DIM, RESET = "\033[1m", "\033[2m", "\033[0m"

rng = random.Random()


def bold(s):
    print(f"\n{BOLD}{s}{RESET}", flush=True)


def dim(s):
    print(f"{DIM}{s}{RESET}", flush=True)


def say(s):
    print(s, flush=True)


# ----------------------------------------------------------------- pricing
# (model, provider, $/1M input tokens, $/1M output tokens). The demo meters
# llm.cost_usd on each span the way an instrumented client would: computed
# from these rates at emission time, not derived by the server afterwards.
MODELS = [
    ("gpt-4o-mini", "openai", 0.15, 0.60),
    ("claude-sonnet-4-5", "anthropic", 3.00, 15.00),
    ("gemini-2.0-flash", "google", 0.10, 0.40),
]


def usd(model, tin, tout):
    _, _, rin, rout = model
    return round(tin * rin / 1e6 + tout * rout / 1e6, 6)


def toks(text):
    return max(1, len(text) // 4)


def new_span_id():
    return f"{rng.getrandbits(64):016x}"


def new_trace_id(prefix):
    return f"{prefix}-{rng.getrandbits(48):012x}"


def span(trace, parent, name, service, t0, t1, status="ok", attrs=None):
    return {
        "trace_id": trace,
        "span_id": new_span_id(),
        "parent_span_id": parent,
        "name": name,
        "service": service,
        "status": status,
        "start_time_ns": int(t0 * NS),
        "end_time_ns": int(t1 * NS),
        "attributes": attrs or {},
        "events": [],
    }


def text_msg(role, content):
    return {"role": role, "parts": [{"type": "text", "content": content}]}


# ------------------------------------------------------------------- HTTP
class Api:
    def __init__(self, base):
        self.base = base

    def post_spans(self, spans):
        body = json.dumps({"spans": spans}).encode()
        req = urllib.request.Request(
            self.base + "/v1/spans",
            data=body,
            headers={"Content-Type": "application/json"},
        )
        for attempt in (1, 2):
            try:
                with urllib.request.urlopen(req, timeout=10) as r:
                    return json.loads(r.read())
            except (urllib.error.URLError, OSError):
                if attempt == 2:
                    raise
                time.sleep(0.5)

    def get(self, path):
        with urllib.request.urlopen(self.base + path, timeout=10) as r:
            return json.loads(r.read())


# -------------------------------------------------------------- scheduler
class Sched:
    """Spans waiting for their end time. A span is posted only after it
    'finished', which is what makes the stream read as live."""

    def __init__(self):
        self.q = []
        self.seq = 0
        self.errors = 0
        self.emitted = 0

    def put(self, post_at, sp):
        if sp["status"] == "error":
            self.errors += 1
        self.emitted += 1
        heapq.heappush(self.q, (post_at, self.seq, sp))
        self.seq += 1

    def due(self, now):
        out = []
        while self.q and self.q[0][0] <= now:
            out.append(heapq.heappop(self.q)[2])
        return out


# ---------------------------------------------------------- support-agent
SYSTEM_PROMPT = (
    "You are the support agent for Traza Cloud. Answer from the knowledge "
    "base, cite order and invoice ids exactly, and escalate anything "
    "involving refunds over $500."
)

SCRIPTS = [
    [
        ("Hi, my order #58231 hasn't shipped yet. It's been five days.",
         "I can see order 58231 in our system. It's held at the warehouse "
         "pending a stock check on one item. I've flagged it for priority "
         "dispatch — you should see movement within 24 hours."),
        ("Can you just split the order and ship what you have?",
         "Done. The in-stock items ship today; the backordered desk lamp "
         "follows next week. You'll get two tracking numbers by email."),
        ("Great, thanks. That works for me.",
         "Happy to help. The first tracking number should arrive within "
         "the hour. Anything else?"),
    ],
    [
        ("I was charged twice for my March invoice.",
         "You're right — I see two captures for invoice INV-2203 on March "
         "4th. The duplicate was a payment-gateway retry that should not "
         "have settled. I've issued a refund; it lands in 3-5 business days."),
        ("Will I get a confirmation of the refund?",
         "Yes — credit note CN-1174 was just emailed to your billing "
         "contact, and the refund reference is RF-88012."),
        ("Perfect. Can you also switch us to annual billing?",
         "Scheduled: your account moves to annual billing at the next "
         "renewal on the 28th, with the 15% annual discount applied."),
    ],
    [
        ("Our webhook deliveries started failing this morning with 401s.",
         "Your signing secret was rotated at 06:12 UTC by an admin on your "
         "account. Deliveries signed with the old secret will 401. Update "
         "the verifier to the new secret, or roll the rotation back."),
        ("That explains it. Can you resend the failed deliveries?",
         "Queued — 47 failed deliveries from the last 6 hours will be "
         "retried with exponential backoff, starting now."),
        ("How do we avoid this next time?",
         "Enable overlapping secrets in webhook settings: both the old and "
         "new secret verify for 24 hours after a rotation."),
        ("Enabled. Thanks for the quick diagnosis.",
         "Any time. The retry queue shows 12 of 47 already delivered."),
    ],
    [
        ("I want to downgrade from Team to Starter before renewal.",
         "Your renewal is on the 28th. I've scheduled the downgrade to "
         "take effect then, so you keep Team features until the date you "
         "have already paid for."),
        ("Do I lose my usage history when the downgrade lands?",
         "No — history is retained on every plan. Only seat count and SSO "
         "change: Starter allows 3 seats and email-based login."),
        ("We have 5 seats today. What happens to the extra two?",
         "The two seats added most recently are deactivated, not deleted. "
         "Reactivating them later restores their history."),
    ],
]


class Chats:
    """A pool of concurrent multi-turn support conversations. Each turn is
    one trace: agent.turn root, an optional kb.search tool call, then the
    model call carrying the conversation content."""

    def __init__(self, sched, n):
        self.sched = sched
        self.active = []
        self.started = 0
        self.next_no = 1041
        now = time.time()
        for i in range(n):
            self.spawn(now + 0.2 + 0.45 * i)

    def spawn(self, at):
        self.active.append({
            "session": f"chat-{self.next_no}",
            "script": SCRIPTS[self.started % len(SCRIPTS)],
            "turn": 0,
            "model": MODELS[self.started % len(MODELS)],
            "history": [],
            "next_at": at,
        })
        self.next_no += 1 + rng.randrange(3)
        self.started += 1

    def step(self, now):
        for chat in list(self.active):
            if now < chat["next_at"]:
                continue
            end = self.emit_turn(chat, now)
            chat["turn"] += 1
            if chat["turn"] >= len(chat["script"]):
                self.active.remove(chat)
                self.spawn(end + rng.uniform(0.5, 2.0))
            else:
                chat["next_at"] = end + rng.uniform(1.5, 4.0)

    def emit_turn(self, chat, t0):
        user, asst = chat["script"][chat["turn"]]
        session = chat["session"]
        trace = new_trace_id("support")
        root = span(trace, None, "agent.turn", "support-agent", t0, t0,
                    attrs={"session.id": session, "agent.turn": chat["turn"] + 1})
        cursor = t0 + 0.04

        if rng.random() < 0.75:
            dur = rng.uniform(0.12, 0.5)
            failed = rng.random() < 0.02
            attrs = {
                "session.id": session,
                "tool.name": "kb.search",
                "kb.query": " ".join(user.split()[:6]).lower().strip(".,?"),
                "kb.hits": 0 if failed else rng.randrange(1, 9),
            }
            if failed:
                attrs["error.message"] = "kb index shard timed out after 450ms"
            tool = span(trace, root["span_id"], "kb.search", "support-agent",
                        cursor, cursor + dur, "error" if failed else "ok", attrs)
            self.sched.put(cursor + dur, tool)
            cursor += dur + 0.05

        streaming = rng.random() < 0.3
        dur = rng.uniform(0.7, 1.6) * (1.5 if streaming else 1.0)
        model = chat["model"]
        input_msgs = [text_msg("system", SYSTEM_PROMPT)]
        for u, a in chat["history"]:
            input_msgs.append(text_msg("user", u))
            input_msgs.append(text_msg("assistant", a))
        input_msgs.append(text_msg("user", user))
        tin = (toks(SYSTEM_PROMPT)
               + sum(toks(u) + toks(a) for u, a in chat["history"])
               + toks(user) + rng.randrange(10, 40))
        tout = toks(asst) + rng.randrange(5, 25)
        attrs = {
            "session.id": session,
            "gen_ai.operation.name": "chat",
            "gen_ai.provider.name": model[1],
            "gen_ai.request.model": model[0],
            "gen_ai.response.model": model[0],
            "gen_ai.usage.input_tokens": tin,
            "gen_ai.usage.output_tokens": tout,
            "llm.cost_usd": usd(model, tin, tout),
            "gen_ai.input.messages": json.dumps(input_msgs),
            "gen_ai.output.messages": json.dumps([text_msg("assistant", asst)]),
        }
        if streaming:
            attrs["llm.is_streaming"] = True
        failed = rng.random() < 0.01
        if failed:
            attrs["error.message"] = "upstream timed out mid-stream"
        llm = span(trace, root["span_id"], f"{model[1]}.chat", "support-agent",
                   cursor, cursor + dur, "error" if failed else "ok", attrs)
        self.sched.put(cursor + dur, llm)
        chat["history"].append((user, asst))

        root_end = cursor + dur + 0.03
        root["end_time_ns"] = int(root_end * NS)
        self.sched.put(root_end, root)
        return root_end


# ---------------------------------------------------------- research-swarm
TOPICS = [
    "vector database pricing 2026",
    "EU AI act conformity deadlines",
    "wal fsync behaviour across filesystems",
    "agent framework benchmark methodology",
]

DOMAINS = ["docs.example.dev", "arxiv.example.org", "news.example.com",
           "wiki.example.net", "api.example.io"]


class Research:
    """Every 15-20 s, one fan-out trace: a planner model call, then 4-8
    genuinely concurrent workers doing web.fetch calls (with the odd 503
    and retry), then a reduce model call. The waterfall showpiece."""

    def __init__(self, sched, base_url, start):
        self.sched = sched
        self.base_url = base_url
        self.next_at = start + 0.6
        self.notices = []
        self.first_done = None
        self.count = 0

    def step(self, now):
        if now >= self.next_at:
            self.build(now)
            self.next_at = now + rng.uniform(15, 20)
        while self.notices and self.notices[0][0] <= now:
            _, trace, nw, nsp = self.notices.pop(0)
            say(f"  research fan-out landed: {nw} concurrent workers, {nsp} spans")
            say(f"    {self.base_url}/#/trace/{trace}")

    def build(self, t0):
        trace = new_trace_id("research")
        topic = TOPICS[self.count % len(TOPICS)]
        self.count += 1
        spans = []

        root = span(trace, None, "research.run", "research-swarm", t0, t0,
                    attrs={"research.topic": topic})

        plan_dur = rng.uniform(0.7, 1.1)
        model = MODELS[1]
        tin, tout = rng.randrange(400, 900), rng.randrange(150, 350)
        nw = rng.randrange(4, 9)
        planner = span(
            trace, root["span_id"], "anthropic.chat", "research-swarm",
            t0 + 0.02, t0 + 0.02 + plan_dur, "ok", {
                "swarm.role": "planner",
                "gen_ai.operation.name": "chat",
                "gen_ai.provider.name": model[1],
                "gen_ai.request.model": model[0],
                "gen_ai.response.model": model[0],
                "gen_ai.usage.input_tokens": tin,
                "gen_ai.usage.output_tokens": tout,
                "llm.cost_usd": usd(model, tin, tout),
                "gen_ai.input.messages": json.dumps([text_msg(
                    "user", f"Research task: {topic}. Split it into "
                            "independent subtopics, one per worker.")]),
                "gen_ai.output.messages": json.dumps([text_msg(
                    "assistant", f"Plan: fan out {nw} workers, one per "
                                 "subtopic; each fetches primary sources; "
                                 "reduce step synthesizes a brief.")]),
            })
        spans.append(planner)

        fan_start = t0 + 0.02 + plan_dur
        worker_ends = []
        for i in range(nw):
            w0 = fan_start + rng.uniform(0.02, 0.25)
            w1 = w0 + rng.uniform(1.0, 2.4)
            worker_ends.append(w1)
            worker = span(trace, root["span_id"], "swarm.worker",
                          "research-swarm", w0, w1, "ok",
                          {"swarm.role": "worker", "swarm.worker": i,
                           "research.topic": topic})
            spans.append(worker)

            fail_first = rng.random() < 0.12
            n_fetch = rng.randrange(1, 3) + (1 if fail_first else 0)
            cur = w0 + 0.05
            seg = (w1 - 0.05 - cur) / n_fetch
            for f in range(n_fetch):
                f1 = cur + seg * rng.uniform(0.6, 0.95)
                err = fail_first and f == 0
                attrs = {
                    "tool.name": "web.fetch",
                    "url.full": f"https://{rng.choice(DOMAINS)}/"
                                f"{topic.split()[0]}/{rng.getrandbits(24):06x}",
                    "http.response.status_code": 503 if err else 200,
                }
                if err:
                    attrs["error.message"] = "503 service unavailable; retrying"
                if fail_first and f == 1:
                    attrs["http.request.resend_count"] = 1
                spans.append(span(trace, worker["span_id"], "web.fetch",
                                  "research-swarm", cur, f1,
                                  "error" if err else "ok", attrs))
                cur += seg

        r0 = max(worker_ends) + 0.05
        r1 = r0 + rng.uniform(0.6, 1.0)
        model = MODELS[2]
        tin = rng.randrange(3000, 6000)
        tout = rng.randrange(300, 700)
        spans.append(span(
            trace, root["span_id"], "google.chat", "research-swarm",
            r0, r1, "ok", {
                "swarm.role": "reduce",
                "gen_ai.operation.name": "chat",
                "gen_ai.provider.name": model[1],
                "gen_ai.request.model": model[0],
                "gen_ai.response.model": model[0],
                "gen_ai.usage.input_tokens": tin,
                "gen_ai.usage.output_tokens": tout,
                "llm.cost_usd": usd(model, tin, tout),
                "gen_ai.input.messages": json.dumps([text_msg(
                    "user", f"Synthesize the {nw} worker findings on "
                            f"'{topic}' into a sourced brief.")]),
                "gen_ai.output.messages": json.dumps([text_msg(
                    "assistant", f"Brief on {topic}: findings converge on "
                                 "three points; two sources conflict on "
                                 "dates and are flagged.")]),
            }))

        root_end = r1 + 0.03
        root["end_time_ns"] = int(root_end * NS)
        root["attributes"]["research.workers"] = nw
        spans.append(root)
        for sp in spans:
            self.sched.put(sp["end_time_ns"] / NS, sp)
        self.notices.append((root_end, trace, nw, len(spans)))
        if self.first_done is None:
            self.first_done = (trace, root_end)


# -------------------------------------------------------------- code-agent
TASKS = [
    "fix flaky retry test in ingest",
    "add cursor paging to the export client",
    "migrate config parser off deprecated API",
    "profile the segment compactor",
]

FILES = ["src/ingest.rs", "src/wal.rs", "src/segment.rs", "tests/http_api.rs",
         "src/compactor.rs", "ui/src/lib/spans.js"]


class Coder:
    """Tool-heavy traces every 4-7 s: file reads, a model call, a test run
    that sometimes fails, a diff. Longer durations than the chat turns."""

    def __init__(self, sched, start):
        self.sched = sched
        self.next_at = start + 1.3
        self.count = 0

    def step(self, now):
        if now >= self.next_at:
            self.build(now)
            self.next_at = now + rng.uniform(4, 7)

    def build(self, t0):
        trace = new_trace_id("code")
        task = TASKS[self.count % len(TASKS)]
        self.count += 1
        root = span(trace, None, "code.task", "code-agent", t0, t0,
                    attrs={"task.description": task})
        cur = t0 + 0.03

        for _ in range(rng.randrange(2, 5)):
            d = rng.uniform(0.01, 0.08)
            self.sched.put(cur + d, span(
                trace, root["span_id"], "fs.read", "code-agent", cur, cur + d,
                "ok", {"tool.name": "fs.read", "file.path": rng.choice(FILES)}))
            cur += d + rng.uniform(0.01, 0.05)

        model = MODELS[self.count % 2]  # alternates gpt-4o-mini / claude
        d = rng.uniform(1.2, 3.0)
        tin, tout = rng.randrange(1500, 6000), rng.randrange(200, 900)
        self.sched.put(cur + d, span(
            trace, root["span_id"], f"{model[1]}.chat", "code-agent",
            cur, cur + d, "ok", {
                "gen_ai.operation.name": "chat",
                "gen_ai.provider.name": model[1],
                "gen_ai.request.model": model[0],
                "gen_ai.response.model": model[0],
                "gen_ai.usage.input_tokens": tin,
                "gen_ai.usage.output_tokens": tout,
                "llm.cost_usd": usd(model, tin, tout),
                "task.description": task,
            }))
        cur += d + 0.05

        d = rng.uniform(1.0, 3.5)
        total = rng.randrange(40, 220)
        failed = rng.random() < 0.10
        nfail = rng.randrange(1, 4) if failed else 0
        attrs = {"tool.name": "tests.run", "tests.total": total,
                 "tests.failed": nfail}
        if failed:
            attrs["error.message"] = f"{nfail} of {total} tests failed"
        self.sched.put(cur + d, span(
            trace, root["span_id"], "tests.run", "code-agent", cur, cur + d,
            "error" if failed else "ok", attrs))
        cur += d + 0.03

        d = rng.uniform(0.05, 0.2)
        self.sched.put(cur + d, span(
            trace, root["span_id"], "git.diff", "code-agent", cur, cur + d,
            "ok", {"tool.name": "git.diff",
                   "diff.files_changed": rng.randrange(1, 7)}))
        cur += d

        root["end_time_ns"] = int((cur + 0.04) * NS)
        self.sched.put(cur + 0.04, root)


# ------------------------------------------------------------ verification
def run_verification(api, base, research_trace):
    bold("verify — every number below was read back from the server")
    results = []

    def check(label, value, need, passed):
        results.append(passed)
        say(f"  {label:<36} {str(value):<10} need {need:<7} "
            f"{'ok' if passed else 'FAIL'}")

    sessions = api.get("/v1/sessions")["sessions"]
    check("sessions listed by /v1/sessions", len(sessions), ">= 3",
          len(sessions) >= 3)

    rows = api.get("/v1/stats/llm")["rows"]
    costed = [r for r in rows if (r.get("cost_usd") or 0) > 0]
    check("models with cost_usd > 0", len(costed), ">= 1", len(costed) >= 1)

    spans = api.get("/v1/traces/" + research_trace)["spans"]
    check("spans in the research trace", len(spans), ">= 6", len(spans) >= 6)

    workers = [s for s in spans if s["name"] == "swarm.worker"]
    edges = sorted([(s["start_time_ns"], 1) for s in workers]
                   + [(s["end_time_ns"], -1) for s in workers],
                   key=lambda e: (e[0], e[1]))
    peak = cur = 0
    for _, d in edges:
        cur += d
        peak = max(peak, cur)
    check("workers running concurrently", f"{peak}/{len(workers)}", ">= 2",
          peak >= 2)

    # The conversation view parses gen_ai.input/output.messages as JSON
    # arrays of {role, parts:[{type,content}]} (ui/src/lib/spans.js). Read
    # one session's spans back and count the turns that parse in exactly
    # that shape: every message a dict with a role and a non-empty parts
    # list, every part a dict with 'type' and 'content' keys.
    def message_ok(m):
        return (isinstance(m, dict) and "role" in m
                and isinstance(m.get("parts"), list) and m["parts"]
                and all(isinstance(p, dict) and "type" in p and "content" in p
                        for p in m["parts"]))

    turns = 0
    if sessions:
        chat_spans = api.get("/v1/spans?session="
                             + sessions[0]["session_id"] + "&limit=100")
        for s in chat_spans["spans"]:
            raw = s["attributes"].get("gen_ai.input.messages")
            if not isinstance(raw, str):
                continue
            try:
                msgs = json.loads(raw)
            except ValueError:
                continue
            if (isinstance(msgs, list) and msgs
                    and all(message_ok(m) for m in msgs)):
                turns += 1
    check("parseable chat turns in a session", turns, ">= 1", turns >= 1)

    if all(results) and sessions:
        dim(f"  a conversation to open: "
            f"{base}/#/conversation/sessions/{sessions[0]['session_id']}")
    return all(results)


def llm_cost_total(api):
    rows = api.get("/v1/stats/llm")["rows"]
    return sum(r.get("cost_usd") or 0 for r in rows), rows


# -------------------------------------------------------------------- main
def main():
    port = sys.argv[1] if len(sys.argv) > 1 else os.environ.get(
        "TRAZA_DEMO_PORT", "8124")
    base = f"http://127.0.0.1:{port}"
    api = Api(base)

    duration = float(os.environ.get("TRAZA_SWARM_SECONDS", "0") or 0)
    if 0 < duration < 8:
        dim(f"  TRAZA_SWARM_SECONDS={duration:g} is below the 8 s "
            "verification window; running 8 s")
        duration = 8.0
    n_chats = max(3, int(os.environ.get("TRAZA_SWARM_CHATS", "4")))

    def on_term(signum, frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, on_term)

    sched = Sched()
    start = time.time()
    chats = Chats(sched, n_chats)
    research = Research(sched, base, start)
    coder = Coder(sched, start)

    dim(f"  streaming three services: support-agent ({n_chats} concurrent "
        "chats), research-swarm (fan-out every 15-20 s), code-agent "
        "(tool-heavy). Ctrl-C for the summary."
        if duration == 0 else
        f"  streaming three services for {duration:g} s: support-agent "
        f"({n_chats} concurrent chats), research-swarm, code-agent.")

    acked = 0
    verified = False
    verify_ok = True
    interrupted = False
    last_status_t, last_status_acked = start, 0

    try:
        while True:
            now = time.time()
            chats.step(now)
            research.step(now)
            coder.step(now)

            batch = sched.due(now)
            if batch:
                resp = api.post_spans(batch)
                acked += resp.get("accepted", 0)

            if now - last_status_t >= 3.0:
                rate = (acked - last_status_acked) / (now - last_status_t)
                try:
                    cost, _ = llm_cost_total(api)
                    cost_txt = f"llm cost ${cost:.4f}"
                except OSError:
                    cost_txt = "llm cost unavailable"
                say(f"  {now - start:5.1f}s  spans acked {acked} "
                    f"({rate:.1f}/s)  sessions started {chats.started}  "
                    f"{cost_txt}")
                last_status_t, last_status_acked = now, acked

            if (not verified and now - start >= 6.0 and research.first_done
                    and now >= research.first_done[1] + 0.5):
                verified = True
                verify_ok = run_verification(api, base, research.first_done[0])
                if not verify_ok:
                    break

            if duration and now - start >= duration and verified:
                break
            if duration and now - start >= duration + 15:
                say("  the verification window never opened "
                    "(no research trace completed); failing")
                verify_ok = False
                break

            time.sleep(0.3)
    except KeyboardInterrupt:
        interrupted = True

    elapsed = time.time() - start
    bold("summary")
    say("  server-read, live:")
    say(f"    spans acked {acked} — summed from the server's /v1/spans "
        "responses")
    try:
        stored = len(api.get("/v1/sessions")["sessions"])
        say(f"    sessions stored {stored} (/v1/sessions)")
    except OSError as e:
        dim(f"    could not read /v1/sessions: {e}")
    try:
        cost, rows = llm_cost_total(api)
        say(f"    total llm cost ${cost:.4f} across {len(rows)} models "
            "(/v1/stats/llm)")
        if rows:
            top = rows[0]
            say(f"    top model by spend: {top['key']}  "
                f"${top.get('cost_usd', 0):.4f}  "
                f"({top.get('llm_calls', 0)} calls, "
                f"{top.get('total_tokens', 0)} tokens)")
    except OSError as e:
        dim(f"    could not read /v1/stats/llm: {e}")
    say("  generator's own tallies, emission-side:")
    say(f"    ran {elapsed:.1f}s   sessions started {chats.started}   "
        f"research fan-outs {research.count}")
    say(f"    error spans emitted {sched.errors}   still in flight at exit "
        f"{len(sched.q)}")
    if interrupted and not verified:
        dim("  interrupted before the 6 s verification gate; "
            "verification skipped")

    sys.exit(0 if verify_ok else 1)


if __name__ == "__main__":
    main()
