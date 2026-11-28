"""The research run the README and the engine's replay and fork guides print,
against a real traza-server.

``dr-0917`` is built span for span as the site's demo script records it: a
coordinator on claude-opus-5-5 hands a web search to a web-researcher
sub-agent on claude-haiku-5-5 through its ``Agent`` tool, then reads the
sources with ``WebFetch``; the sub-agent searches with ``WebSearch``. The
harness runs every tool itself, so each is a step. Prompts are OpenLLMetry's
indexed layout, with the cache counters Anthropic reports. The README's replay
loop runs here as written, with a few lines of urllib standing in for the
Anthropic SDK's ``messages.create`` (the package depends on nothing), and the
outputs the documents show are asserted exactly: the served brief, the diff
against the deduping fork, recall, and the tape test's failure.
"""

from __future__ import annotations

import hashlib
import json
import os
import sys
import tempfile
import unittest
import urllib.error
import urllib.request
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from _server import Server  # noqa: E402

from traza import Client  # noqa: E402
from traza.testing import TapeMismatch, replay_test  # noqa: E402

# 2026-01-01T00:00:00Z: old enough that both runs read as finished.
T0 = 1_767_225_600_000_000_000
MS = 1_000_000

COORDINATOR = ("You are a research coordinator. Delegate web searches to the web-researcher subagent, read the "
               "sources it finds with WebFetch, then write a one-page brief that cites every source by URL.")
WORKER = ("You are web-researcher. Search the web and reply with the most relevant primary sources, one line "
          "each, with their URLs.")
TASK = "Write a one-page brief on solid-state battery readiness for EVs in 2027, with sources."
BRIEF = "Find 2026 primary sources on solid-state battery pilot production, yields and cost per kWh."
QUERY = "solid-state battery pilot line yield cost per kWh 2026"
FETCH_PROMPT = "List the 2026 figures on pilot lines, yields, cost per kWh and sample cells."
URLS = [
    "https://www.example-energy.org/2026/solid-state-pilot-lines",
    "https://www.example-batteries.com/reports/ssb-cost-2026",
    "https://www.example-auto.net/news/2026-ssb-sample-cells",
]
PAGES = [
    "Pilot lines (2026): three cell makers ran solid-state pilot lines at 0.2 to 1 GWh a year. First-pass yields "
    "were reported between 60% and 75%.",
    "Cost (2026): solid-state cells were estimated at $180 to $240 per kWh, against about $95 for LFP. The gap "
    "narrows only with sulfide electrolyte scale-up after 2028.",
    "Samples (2026): two automakers began road tests of B-sample solid-state cells late in the year. Neither "
    "committed to a 2027 production vehicle.",
]
PAGE_TOKENS = [6_200, 5_400, 4_800]
SEARCH = "\n".join(f"{index}. {url}" for index, url in enumerate(URLS, 1))
FINDINGS = "Sources:\n" + "\n".join(f"- {url}: {page.split(':')[0]}" for url, page in zip(URLS, PAGES))
FINAL = ("Solid-state batteries in 2027: pilot scale, not production. Pilot lines ran at 0.2 to 1 GWh a year in "
         "2026 with 60 to 75% first-pass yields; cells cost $180 to $240 per kWh against about $95 for LFP; "
         "automakers are road-testing B-samples but none has committed to a 2027 vehicle. Expect limited premium "
         "launches after 2028.")
AGENTS = {
    "coordinator": {"model": "claude-opus-5-5", "agent": "research-coordinator", "system": COORDINATOR, "prefix": 9_800},
    "worker": {"model": "claude-haiku-5-5", "agent": "web-researcher", "system": WORKER, "prefix": 2_600},
}
# The list rates the guides' server was started with (``--pricing``).
PRICING = {"models": {
    "claude-opus-5-5": {"input_per_mtok": 4.0, "output_per_mtok": 20.0,
                        "cache_read_per_mtok": 0.2, "cache_write_per_mtok": 5.0},
    "claude-haiku-5-5": {"input_per_mtok": 0.1, "output_per_mtok": 0.5,
                         "cache_read_per_mtok": 0.01, "cache_write_per_mtok": 0.125},
}}


def sha(text):
    return hashlib.sha256(text.encode()).hexdigest()


def stringify(value):
    """``JSON.stringify``: keys in insertion order, no spaces."""
    return json.dumps(value, separators=(",", ":"))


def dedupe_note(url):
    return f"Already fetched {url} earlier in this run; reusing that result."


def plan(fetches, dedupe=False):
    """The run as the harness lives it: model calls (``ask``/``say``) and tool calls (``run``)."""
    steps = [
        {"agent": "coordinator", "tool": "Agent",
         "input": {"description": "Battery sources", "prompt": BRIEF, "subagent_type": "web-researcher"}},
        {"agent": "worker", "tool": "WebSearch", "input": {"query": QUERY}},
        {"run": "WebSearch", "result": SEARCH, "tokens": 380},
        {"agent": "worker", "text": FINDINGS, "final": True},
    ]
    seen = set()
    for page in fetches:
        steps.append({"agent": "coordinator", "tool": "WebFetch", "input": {"url": URLS[page], "prompt": FETCH_PROMPT}})
        if dedupe and page in seen:
            steps.append({"run": "WebFetch", "result": dedupe_note(URLS[page]), "tokens": 40, "deduped": True})
        else:
            steps.append({"run": "WebFetch", "result": PAGES[page], "tokens": PAGE_TOKENS[page]})
        seen.add(page)
    steps.append({"agent": "coordinator", "text": FINAL})
    return steps


def calls_of(steps, key):
    """Each agent's conversation as the harness sends it, with the usage the provider reports."""
    convo = {
        "coordinator": {"messages": [{"role": "user", "content": TASK}], "ctx": 40, "cached": None},
        "worker": {"messages": [{"role": "user", "content": BRIEF}], "ctx": 30, "cached": None},
    }
    calls, pending, task_id, asked = [], None, None, 0
    for step in steps:
        if "agent" in step:
            who = step["agent"]
            c = convo[who]
            prefix = AGENTS[who]["prefix"]
            read = 0 if c["cached"] is None else prefix + c["cached"]
            write = prefix + c["ctx"] if c["cached"] is None else c["ctx"] - c["cached"]
            sent = json.loads(json.dumps(c["messages"]))
            if "tool" in step:
                asked += 1
                use = {"type": "tool_use", "id": "toolu_" + sha(f"{key}:{asked}")[:22], "name": step["tool"],
                       "input": step["input"]}
                content = [use]
                pending = (who, use["id"])
                if step["tool"] == "Agent":
                    task_id = use["id"]
                output = 60 + -(-len(stringify(step["input"])) // 4)
            else:
                content = [{"type": "text", "text": step["text"]}]
                output = 1_900 if who == "coordinator" else 720
            c["cached"] = c["ctx"]
            c["ctx"] += output
            c["messages"].append({"role": "assistant", "content": content})
            calls.append({**step, "sent": sent, "content": content, "usage": [3 + asked % 5, output, read, write]})
            if step.get("final"):
                convo["coordinator"]["messages"].append({"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": task_id, "content": step["text"]}]})
                convo["coordinator"]["ctx"] += output
                calls.append({"closes": True, "result": step["text"]})
        else:
            who, use_id = pending
            convo[who]["messages"].append({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": use_id, "content": step["result"]}]})
            convo[who]["ctx"] += step["tokens"]
            calls.append({**step, "agent": who, "id": use_id})
    return calls


def spans_of(run, key, start, calls, extra=None):
    """The run's spans in Traza's native JSON; ``extra`` lands on every span (a fork's tags)."""
    trace = sha(f"trace:{key}")[:32]
    serial = [0]

    def next_id():
        serial[0] += 1
        return sha(f"span:{key}:{serial[0]}")[:16]

    def attrs(more):
        return {"session.id": run, **(extra or {}), **more}

    def span(span_id, parent, name, begin, ms, attributes):
        return {"trace_id": trace, "span_id": span_id, "parent_span_id": parent, "name": name,
                "service": "research-coordinator", "status": "ok",
                "start_time_unix_nano": begin, "end_time_unix_nano": begin + ms * MS, "attributes": attributes}

    inputs = {c["content"][0]["id"]: c["input"] for c in calls if "content" in c and "tool" in c}
    root = next_id()
    spans, clock, task, sub = [], start + 5 * MS, None, None
    for c in calls:
        if c.get("closes"):
            sub["end_time_unix_nano"] = clock
            task["end_time_unix_nano"] = clock + 2 * MS
            task["attributes"]["gen_ai.tool.call.result"] = c["result"]
            clock += 10 * MS
        elif "content" in c:
            who = AGENTS[c["agent"]]
            text = "text" in c
            ms = (21_400 if text else 3_600) if c["agent"] == "coordinator" else (4_100 if text else 1_200)
            a = attrs({
                "gen_ai.operation.name": "chat", "gen_ai.system": "anthropic", "gen_ai.provider.name": "anthropic",
                "gen_ai.request.model": who["model"], "gen_ai.response.model": who["model"],
                "gen_ai.agent.name": who["agent"],
                "gen_ai.usage.input_tokens": c["usage"][0], "gen_ai.usage.output_tokens": c["usage"][1],
                "gen_ai.usage.cache_read.input_tokens": c["usage"][2],
                "gen_ai.usage.cache_creation.input_tokens": c["usage"][3],
                "gen_ai.prompt.0.role": "system", "gen_ai.prompt.0.content": who["system"],
            })
            for index, message in enumerate(c["sent"], 1):
                a[f"gen_ai.prompt.{index}.role"] = message["role"]
                content = message["content"]
                a[f"gen_ai.prompt.{index}.content"] = content if isinstance(content, str) else stringify(content)
            a["gen_ai.completion.0.role"] = "assistant"
            if text:
                a["gen_ai.completion.0.content"] = c["text"]
                a["gen_ai.completion.0.finish_reason"] = "end_turn"
            else:
                use = c["content"][0]
                a["gen_ai.completion.0.finish_reason"] = "tool_use"
                a["gen_ai.completion.0.tool_calls.0.id"] = use["id"]
                a["gen_ai.completion.0.tool_calls.0.name"] = use["name"]
                a["gen_ai.completion.0.tool_calls.0.arguments"] = stringify(use["input"])
            parent = sub["span_id"] if c["agent"] == "worker" else root
            spans.append(span(next_id(), parent, f"chat {who['model']}", clock, ms, a))
            clock += (ms + 40) * MS
            if c.get("tool") == "Agent":
                use = c["content"][0]
                task = span(next_id(), root, "execute_tool Agent", clock, 0, attrs({
                    "gen_ai.operation.name": "execute_tool", "gen_ai.tool.name": "Agent",
                    "gen_ai.agent.name": "research-coordinator", "gen_ai.tool.call.id": use["id"],
                    "gen_ai.tool.call.arguments": stringify(use["input"])}))
                spans.append(task)
                clock += 5 * MS
                sub = span(next_id(), task["span_id"], "invoke_agent web-researcher", clock, 0, attrs({
                    "gen_ai.operation.name": "invoke_agent", "gen_ai.agent.name": "web-researcher"}))
                spans.append(sub)
                clock += 5 * MS
        else:
            ms = 900 if c["run"] == "WebSearch" else 2 if c.get("deduped") else 1_700
            parent = sub["span_id"] if c["agent"] == "worker" else root
            spans.append(span(next_id(), parent, f"execute_tool {c['run']}", clock, ms, attrs({
                "gen_ai.operation.name": "execute_tool", "gen_ai.tool.name": c["run"],
                "gen_ai.agent.name": AGENTS[c["agent"]]["agent"], "gen_ai.tool.call.id": c["id"],
                "gen_ai.tool.call.arguments": stringify(inputs[c["id"]]), "gen_ai.tool.call.result": c["result"]})))
            clock += (ms + 40) * MS
    spans.insert(0, span(root, None, "invoke_agent research-coordinator", start, (clock + 10 * MS - start) // MS,
                         attrs({"gen_ai.operation.name": "invoke_agent", "gen_ai.agent.name": "research-coordinator"})))
    return spans


class Messages:
    """The Anthropic SDK's ``messages.create``, with urllib: the README's loop runs against it unchanged."""

    def __init__(self, base_url, api_key):
        self.base_url, self.api_key = base_url, api_key

    def create(self, **body):
        request = urllib.request.Request(
            f"{self.base_url}/v1/messages", data=json.dumps(body).encode(), method="POST",
            headers={"Content-Type": "application/json", "x-api-key": self.api_key,
                     "anthropic-version": "2023-06-01"})
        try:
            with urllib.request.urlopen(request, timeout=10) as response:
                answer = json.loads(response.read())
        except urllib.error.HTTPError as error:
            error.close()   # a refusal: the SDK raises, and the tape test reads the replay
            raise
        blocks = [SimpleNamespace(**block) for block in answer["content"]]
        return SimpleNamespace(content=blocks)


class Anthropic:
    def __init__(self, base_url, api_key):
        self.messages = Messages(base_url, api_key)


def assistant(blocks):
    """What the SDK sends back for an answer's content blocks."""
    return [{k: v for k, v in vars(block).items()} for block in blocks]


AGENT = {"name": "Agent", "input_schema": {"type": "object", "properties": {"prompt": {"type": "string"}}}}
WEB_SEARCH = {"name": "WebSearch", "input_schema": {"type": "object", "properties": {"query": {"type": "string"}}}}
WEB_FETCH = {"name": "WebFetch", "input_schema": {"type": "object", "properties": {"url": {"type": "string"}}}}


def research(llm, tool_result, dedupe=False):
    """The README's loop; ``dedupe`` is the harness that refuses to fetch a URL twice."""
    fetched = set()

    def run(model, system, task, tools):
        messages = [{"role": "user", "content": task}]
        while True:
            answer = llm.messages.create(model=model, max_tokens=8192, system=system, messages=messages, tools=tools)
            messages.append({"role": "assistant", "content": assistant(answer.content)})
            calls = [block for block in answer.content if block.type == "tool_use"]
            if not calls:
                return answer.content[0].text
            results = []
            for call in calls:
                if call.name == "Agent":     # the sub-agent's calls are on the tape too
                    result = run("claude-haiku-5-5", WORKER, call.input["prompt"], [WEB_SEARCH])
                elif dedupe and call.name == "WebFetch" and call.input["url"] in fetched:
                    result = dedupe_note(call.input["url"])
                else:                        # the recorded result, not a live search or fetch
                    fetched.add(call.input.get("url"))
                    result = tool_result(call.name).result
                results.append({"type": "tool_result", "tool_use_id": call.id, "content": result})
            messages.append({"role": "user", "content": results})

    return run("claude-opus-5-5", COORDINATOR, TASK, [AGENT, WEB_FETCH])


SERVER = None
CLIENT = None
FETCHES = [0, 1, 0, 2, 1]


def setUpModule():
    global SERVER, CLIENT
    rates = tempfile.NamedTemporaryFile("w", suffix=".json", delete=False)
    json.dump(PRICING, rates)
    rates.close()
    unittest.addModuleCleanup(os.unlink, rates.name)
    SERVER = Server(args=["--pricing", rates.name])
    unittest.addModuleCleanup(SERVER.stop)
    CLIENT = Client(SERVER.url)
    CLIENT.request("POST", "/v1/spans", body=spans_of("dr-0917", "dr-0917", T0, calls_of(plan(FETCHES), "dr-0917")))
    # The deduping harness, tried as a rewind (live after step 11) and run on
    # live from its cut at 12, recorded under its own session with the
    # fork's tags.
    rewind = CLIENT.replay("dr-0917", live_after=11)
    live = calls_of(plan([0, 1, 0, 2], dedupe=True), "dr-0917")
    CLIENT.request("POST", "/v1/spans", body=spans_of(
        "dr-0917-dedupe", "dr-0917-dedupe", T0 + 90 * 60_000 * MS, live, extra=rewind.fork.attributes))


class TheReadmeAndTheGuides(unittest.TestCase):
    def test_the_readme_replay_loop_is_answered_from_the_recording(self):
        traza = CLIENT
        replay = traza.replay("dr-0917")       # strict by default
        llm = Anthropic(base_url=replay.anthropic_base_url, api_key=traza.token or "traza-open")
        self.assertEqual(research(llm, replay.next_tool), FINAL)
        status = replay.status()
        self.assertEqual((status.served, status.remaining, status.diverged), (9, 0, None))
        self.assertEqual([call.step for call in status.calls], [1, 4, 6, 7, 9, 11, 13, 15, 17])
        self.assertTrue(all(call.matched for call in status.calls))

    def test_the_diff_and_recall_are_the_readmes(self):
        diff = CLIENT.diff("dr-0917", "dr-0917-dedupe")
        self.assertEqual(
            diff.first_divergence.explanation,
            "step 12 of a and step 12 of b (tool:WebFetch) received the same input and produced a different "
            "output: response e450b5a73db1 in a, 750af04196bb in b",
        )
        self.assertEqual(
            [(step.op, step.why) for step in diff.aligned][11:],
            [("same", None), ("changed", "decision"), ("changed", "input"), ("same", None),
             ("changed", "input"), ("delete", "structure"), ("delete", "structure")],
        )
        self.assertEqual(diff.delta.steps, -2)
        self.assertAlmostEqual(diff.delta.cost_usd, -0.0682632, places=9)
        found = list(CLIENT.recall("solid-state battery", outcome="any", limit=5))
        self.assertEqual([(run.run, run.outcome, run.score) for run in found],
                         [("dr-0917", "success", 11), ("dr-0917-dedupe", "success", 10)])
        self.assertEqual(found[0].skeleton[:3], ["agent:research-coordinator", "llm:claude-opus-5-5", "tool:Agent"])

    def test_the_deduping_harness_fails_its_tape_test_where_the_fork_guide_says(self):
        def agent(tape):
            return research(Anthropic(**{"base_url": os.environ["ANTHROPIC_BASE_URL"],
                                         "api_key": os.environ["ANTHROPIC_API_KEY"]}),
                            tape.tool_result, dedupe=True)

        env = {"TRAZA_REPLAY_KEY": "open"}
        with mock.patch.dict(os.environ, env):
            with self.assertRaises(TapeMismatch) as caught:
                replay_test(CLIENT, "dr-0917", agent)
        self.assertEqual(
            str(caught.exception),
            "the agent diverged from recorded run dr-0917 at step 13:\n"
            "  message 9 (user): content differs at character 13\n"
            "    recorded: '[{\"content\":\"Pilot lines (2026): three cell makers ran solid-state pilot lines at 0.2 "
            "to 1 GWh a year. First-pass yields…'\n"
            "    got:      '[{\"content\":\"Already fetched https://www.example-energy.org/2026/solid-state-pilot-lines "
            "earlier in this run; reusing th…'\n"
            "  (the recording's request has 10 messages, the agent's 10)",
        )
        # The recorded harness passes the same test.
        with mock.patch.dict(os.environ, env):
            status = replay_test(CLIENT, "dr-0917", lambda tape: research(
                Anthropic(os.environ["ANTHROPIC_BASE_URL"], os.environ["ANTHROPIC_API_KEY"]), tape.tool_result))
        self.assertEqual((status.served, status.remaining), (9, 0))


if __name__ == "__main__":
    unittest.main()
