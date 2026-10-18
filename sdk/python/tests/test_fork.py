"""Forks, tape tests, golden runs and the ``traza`` command, against a real
traza-server.

The weather agent's run is seeded as OpenLLMetry records it. The "real
provider" a fork goes live to is a canned-answer stub on loopback, so the
switch's hand-over is observed exactly — which request reached it, with which
key — and nothing ever leaves the machine. Agents are a few lines of urllib,
the way a provider SDK speaks.
"""

from __future__ import annotations

import contextvars
import io
import json
import os
import subprocess
import sys
import tempfile
import textwrap
import threading
import unittest
import urllib.error
import urllib.request
import xml.etree.ElementTree as ET
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

SRC = Path(__file__).resolve().parents[1] / "src"
sys.path.insert(0, str(SRC))

from _server import Server  # noqa: E402
from _stub import Stub, response  # noqa: E402

import traza  # noqa: E402
from traza import Client  # noqa: E402
from traza.cli import build_parser, main as cli_main  # noqa: E402
from traza.forking import Fork, ForkSpanProcessor, active_fork  # noqa: E402
from traza.models import Divergence, ReplayStatus  # noqa: E402
from traza.testing import (  # noqa: E402
    GoldenResult,
    TapeMismatch,
    TapeTest,
    describe,
    junit_xml,
    replay_key,
    replay_test,
)

T0 = 1_767_225_600_000_000_000
MS = 1_000_000
SYSTEM = "You are a weather assistant."
USER = "What's the weather in Paris?"
RAIN = json.dumps({"forecast": "rain", "celsius": 12})


def span(trace, span_id, name, start_ms, end_ms, attributes):
    return {
        "trace_id": trace, "span_id": span_id, "parent_span_id": None, "name": name,
        "service": "weather-agent", "status": "ok",
        "start_time_ns": T0 + start_ms * MS, "end_time_ns": T0 + end_ms * MS,
        "attributes": attributes,
    }


def weather_run(session, trace, at_ms=0, result=RAIN, extra=None):
    llm = {"gen_ai.system": "openai", "gen_ai.request.model": "gpt-4o", "gen_ai.response.model": "gpt-4o-2024-08-06"}
    spans = [
        span(trace, "llm-1", "openai.chat", at_ms, at_ms + 400, {
            **llm,
            "gen_ai.prompt.0.role": "system", "gen_ai.prompt.0.content": SYSTEM,
            "gen_ai.prompt.1.role": "user", "gen_ai.prompt.1.content": USER,
            "gen_ai.completion.0.role": "assistant", "gen_ai.completion.0.finish_reason": "tool_calls",
            "gen_ai.completion.0.tool_calls.0.id": "call_w1",
            "gen_ai.completion.0.tool_calls.0.name": "get_weather",
            "gen_ai.completion.0.tool_calls.0.arguments": json.dumps({"city": "Paris"}),
        }),
        span(trace, "tool-1", "get_weather.tool", at_ms + 410, at_ms + 450, {
            "traceloop.span.kind": "tool", "traceloop.entity.name": "get_weather",
            "traceloop.entity.input": json.dumps({"city": "Paris"}), "traceloop.entity.output": result,
        }),
        span(trace, "llm-2", "openai.chat", at_ms + 460, at_ms + 900, {
            **llm,
            "gen_ai.prompt.0.role": "system", "gen_ai.prompt.0.content": SYSTEM,
            "gen_ai.prompt.1.role": "user", "gen_ai.prompt.1.content": USER,
            "gen_ai.prompt.2.role": "assistant",
            "gen_ai.prompt.2.tool_calls.0.id": "call_w1",
            "gen_ai.prompt.2.tool_calls.0.name": "get_weather",
            "gen_ai.prompt.2.tool_calls.0.arguments": json.dumps({"city": "Paris"}),
            "gen_ai.prompt.3.role": "tool", "gen_ai.prompt.3.tool_call_id": "call_w1",
            "gen_ai.prompt.3.content": result,
            "gen_ai.completion.0.role": "assistant", "gen_ai.completion.0.finish_reason": "stop",
            "gen_ai.completion.0.content": "It's raining in Paris, 12°C.",
        }),
    ]
    for item in spans:
        item["attributes"]["session.id"] = session
        item["attributes"].update(extra or {})
    return spans


def messages(user=USER, tool_result=None):
    out = [{"role": "system", "content": SYSTEM}, {"role": "user", "content": user}]
    if tool_result is not None:
        out.append({"role": "assistant", "content": None, "tool_calls": [{
            "id": "call_w1", "type": "function",
            "function": {"name": "get_weather", "arguments": json.dumps({"city": "Paris"}, separators=(",", ":"))},
        }]})
        out.append({"role": "tool", "tool_call_id": "call_w1", "content": tool_result})
    return out


def chat(base_url, key, body):
    """The OpenAI SDK's chat-completions call, with urllib."""
    request = urllib.request.Request(
        base_url + "/chat/completions", data=json.dumps(body).encode(), method="POST",
        headers={"Content-Type": "application/json", "Authorization": "Bearer " + key},
    )
    with urllib.request.urlopen(request, timeout=10) as answer:
        return answer.status, json.loads(answer.read())


#: A weather agent as a script: its task is argv[1]; --stop-early makes only
#: the first model call.
AGENT = textwrap.dedent(
    """
    import json, os, sys, urllib.request
    base, key = os.environ["OPENAI_BASE_URL"], os.environ["OPENAI_API_KEY"]
    if os.environ.get("AGENT_KEY_FILE"):
        with open(os.environ["AGENT_KEY_FILE"], "a") as record:
            record.write(key + "\\n")
    task = sys.argv[1]
    def chat(messages):
        body = json.dumps({"model": "gpt-4o", "messages": messages}).encode()
        request = urllib.request.Request(base + "/chat/completions", data=body, method="POST",
            headers={"Content-Type": "application/json", "Authorization": "Bearer " + key})
        with urllib.request.urlopen(request, timeout=10) as answer:
            return json.loads(answer.read())["choices"][0]["message"]
    history = [{"role": "system", "content": "You are a weather assistant."}, {"role": "user", "content": task}]
    first = chat(history)
    if "--stop-early" in sys.argv:
        sys.exit(0)
    call = first["tool_calls"][0]
    history.append({"role": "assistant", "content": None, "tool_calls": [call]})
    history.append({"role": "tool", "tool_call_id": call["id"],
                    "content": json.dumps({"forecast": "rain", "celsius": 12})})
    print(chat(history)["content"])
    """
)

SERVER = None
CLIENT = None
SCRATCH = None


def setUpModule():
    global SERVER, CLIENT, SCRATCH
    SERVER = Server()
    unittest.addModuleCleanup(SERVER.stop)
    CLIENT = Client(SERVER.url)
    CLIENT.request("POST", "/v1/spans", body=weather_run("weather-7", "4bf92f3577b34da6a3ce929d0e0e4736"))
    SCRATCH = tempfile.mkdtemp(prefix="traza-sdk-fork-")
    unittest.addModuleCleanup(lambda: __import__("shutil").rmtree(SCRATCH, ignore_errors=True))
    Path(SCRATCH, "agent.py").write_text(AGENT)


def provider_answer(request):
    """The stub provider: one final answer, whatever it is asked."""
    return response(200, json.dumps({
        "id": "chatcmpl-live", "object": "chat.completion", "created": 1, "model": "gpt-4o-live",
        "choices": [{"index": 0, "finish_reason": "stop",
                     "message": {"role": "assistant", "content": "Live: it is sunny in Paris."}}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
    }).encode(), {"Content-Type": "application/json"})


class ForkCreation(unittest.TestCase):
    def test_a_replay_with_edits_is_a_fork_and_says_so(self):
        replay = CLIENT.replay("weather-7", edits=[{"step": 1, "tool_result": "sunny"}], live_after=1)
        self.assertTrue(replay.fork_id.startswith("fk_"))
        self.assertEqual(replay.fork.parent, "weather-7")
        self.assertEqual(replay.fork.step, 1)
        self.assertEqual([(edit.step, edit.kind) for edit in replay.fork.edits], [(1, "tool_result")])
        self.assertEqual(replay.fork.attributes["traza.fork.id"], replay.fork_id)
        status = replay.status()
        self.assertEqual(status.fork_id, replay.fork_id)
        self.assertEqual(status.fork.edits[0].served, False)
        self.assertEqual(status.exhausted_calls, 0)
        plain = CLIENT.replay("weather-7")
        self.assertIsNone(plain.fork_id)
        self.assertIsNone(plain.fork)

    def test_a_fork_the_run_cannot_take_is_refused_with_the_reason(self):
        with self.assertRaises(traza.TrazaError) as caught:
            CLIENT.replay("weather-7", edits=[{"step": 1, "response": "hi"}])
        self.assertEqual(caught.exception.status, 400)
        self.assertIn("step 1 is a tool call", str(caught.exception))

    def test_the_fork_arguments_are_checked_before_anything_is_sent(self):
        for arguments in ({"at_step": -1}, {"at_step": 0}, {"at_step": 1, "live": False}):
            with self.assertRaises(ValueError):
                Fork(CLIENT, "weather-7", **arguments)
        with self.assertRaises(ValueError):
            Fork(CLIENT, "weather-7", 1, response="a", tool_result="b")
        self.assertEqual(Fork(CLIENT, "weather-7", 2).live_after(), 1)
        self.assertEqual(Fork(CLIENT, "weather-7", 2, response="x").live_after(), 2)
        self.assertEqual(Fork(CLIENT, "weather-7", 2, response="x", live=False).live_after(), None)
        self.assertEqual(Fork(CLIENT, "weather-7", 1, tool_result=None).edits(),
                         [{"step": 1, "tool_result": None}], "a null tool result is an edit")


class ForkGoesLive(unittest.TestCase):
    def test_the_switch_replays_up_to_the_cut_then_hands_over_to_the_provider(self):
        with Stub(provider_answer) as provider:
            before = {key: os.environ.get(key) for key in ("OPENAI_BASE_URL", "OPENAI_API_KEY")}
            fork = traza.fork(
                "weather-7", 1, tool_result={"forecast": "sun", "celsius": 30}, client=CLIENT,
                live_openai_base_url=provider.url + "/v1", live_openai_api_key="sk-real-provider-key",
            )
            with fork:
                self.assertIs(active_fork(), fork)
                base, key = os.environ["OPENAI_BASE_URL"], os.environ["OPENAI_API_KEY"]
                self.assertTrue(base.startswith("http://127.0.0.1:") and base.endswith("/openai/v1"))
                self.assertNotEqual(key, "sk-real-provider-key", "the agent never holds the real key")
                # Step 0 comes from the recording.
                status, first = chat(base, key, {"model": "gpt-4o", "messages": messages()})
                self.assertEqual(status, 200)
                self.assertEqual(first["choices"][0]["message"]["tool_calls"][0]["id"], "call_w1")
                # Step 1, the tool, is the edit.
                tool = fork.tool_result("get_weather")
                self.assertEqual(tool.result, {"forecast": "sun", "celsius": 30})
                self.assertTrue(tool.edited)
                self.assertEqual(len(provider.requests), 0, "nothing has gone live yet")
                # Step 2 is past the cut: the provider answers it.
                edited = json.dumps(tool.result)
                status, live = chat(base, key, {"model": "gpt-4o", "messages": messages(tool_result=edited)})
                self.assertEqual(status, 200)
                self.assertEqual(live["choices"][0]["message"]["content"], "Live: it is sunny in Paris.")
                self.assertTrue(fork.is_live)
                self.assertEqual(len(provider.requests), 1)
                sent = provider.requests[0]
                self.assertEqual(sent.path, "/v1/chat/completions")
                self.assertEqual(sent.headers["authorization"], "Bearer sk-real-provider-key")
                self.assertEqual(json.loads(sent.body)["messages"][3]["content"], edited,
                                 "the provider sees the conversation with the edited result")
                # Past the cut no recorded tool step is served: run the real one.
                self.assertEqual(fork.call_tool("get_weather", lambda: "ran live"), "ran live")
                # A caller without the fork's secret is refused.
                with self.assertRaises(urllib.error.HTTPError) as refused:
                    chat(base, "someone-else", {"model": "gpt-4o", "messages": messages()})
                self.assertEqual(refused.exception.code, 401)
                refused.exception.close()
            self.assertIsNone(active_fork())
            self.assertEqual({key: os.environ.get(key) for key in before}, before, "environment restored")
            self.assertEqual(fork.final_status.fork.live_calls, 2, "one model call and one tool call went live")
            self.assertEqual(fork.final_status.served, 1)

    def test_without_live_the_edit_is_served_and_the_recording_plays_on(self):
        with Fork(CLIENT, "weather-7", 0, response="It is sunny in Paris.", live=False) as fork:
            status, answer = chat(fork.openai_base_url, fork.api_key, {"model": "gpt-4o", "messages": messages()})
            self.assertEqual(answer["choices"][0]["message"]["content"], "It is sunny in Paris.")
            status, answer = chat(fork.openai_base_url, fork.api_key,
                                  {"model": "gpt-4o", "messages": messages(tool_result="whatever")})
            self.assertEqual(answer["choices"][0]["message"]["content"], "It's raining in Paris, 12°C.")
        self.assertFalse(fork.is_live)


class ForksSideBySide(unittest.TestCase):
    """The process environment is one fork's at a time; overlapping forks
    take set_env=False, and none mistakes another's switch for the provider."""

    def setUp(self):
        saved = {key: os.environ.get(key) for key in ("OPENAI_BASE_URL", "OPENAI_API_KEY")}

        def restore():
            for key, value in saved.items():
                if value is None:
                    os.environ.pop(key, None)
                else:
                    os.environ[key] = value

        self.addCleanup(restore)

    def test_a_second_fork_cannot_take_the_environment_and_never_goes_live_to_a_switch(self):
        with Stub(provider_answer) as provider:
            os.environ["OPENAI_BASE_URL"] = provider.url + "/v1"
            os.environ["OPENAI_API_KEY"] = "sk-real-provider-key"
            with Fork(CLIENT, "weather-7", 1, tool_result="sunny") as outer:
                self.assertTrue(os.environ["OPENAI_BASE_URL"].startswith("http://127.0.0.1:"))
                nested = Fork(CLIENT, "weather-7", 1, tool_result="rain")
                with self.assertRaises(RuntimeError) as refused:
                    nested.start()
                self.assertIn("set_env=False", str(refused.exception))
                self.assertIsNone(nested.replay, "refused before a replay was created")
                self.assertTrue(os.environ["OPENAI_API_KEY"].startswith("traza-fork-"), "outer's env untouched")
                # Built while outer owns the environment, it still finds the
                # real provider, not outer's switch and secret.
                inner = Fork(CLIENT, "weather-7", 1, tool_result="rain", set_env=False)
                self.assertEqual(inner.live_openai_base_url, provider.url + "/v1")
                self.assertEqual(inner.live_openai_api_key, "sk-real-provider-key")
                with inner:
                    chat(inner.openai_base_url, inner.api_key, {"model": "gpt-4o", "messages": messages()})
                    self.assertEqual(inner.tool_result("get_weather").result, "rain")
                    status, live = chat(inner.openai_base_url, inner.api_key,
                                        {"model": "gpt-4o", "messages": messages(tool_result="rain")})
                    self.assertEqual(live["choices"][0]["message"]["content"], "Live: it is sunny in Paris.")
                self.assertEqual(len(provider.requests), 1, "inner went live to the provider itself")
                self.assertEqual(provider.requests[0].headers["authorization"], "Bearer sk-real-provider-key")
            self.assertEqual(os.environ["OPENAI_BASE_URL"], provider.url + "/v1", "environment restored")
            self.assertEqual(os.environ["OPENAI_API_KEY"], "sk-real-provider-key")
            # With the block over, the environment is free again.
            with Fork(CLIENT, "weather-7", 1, tool_result="sunny"):
                pass


class FakeSpan:
    def __init__(self):
        self.attributes = {}

    def set_attribute(self, key, value):
        self.attributes[key] = value


class ForkTagsSpans(unittest.TestCase):
    def test_spans_started_inside_a_fork_carry_its_tags_and_session(self):
        processor = ForkSpanProcessor()
        outside = FakeSpan()
        processor.on_start(outside)
        self.assertEqual(outside.attributes, {})
        with Fork(CLIENT, "weather-7", 1, run_id="weather-7-what-if", tag_spans=False) as fork:
            inside = FakeSpan()
            processor.on_start(inside, None)
        self.assertEqual(inside.attributes, {
            "traza.fork.parent": "weather-7", "traza.fork.step": 1, "traza.fork.id": fork.id,
            "session.id": "weather-7-what-if",
        })
        self.assertEqual(fork.env()["TRAZA_FORK_RUN"], "weather-7-what-if")

    def test_a_thread_outside_the_fork_context_is_tagged_only_when_asked(self):
        processor = ForkSpanProcessor()

        def started_on_a_thread(context=None):
            span = FakeSpan()
            target = processor.on_start if context is None else (lambda s: context.run(processor.on_start, s))
            worker = threading.Thread(target=target, args=(span,))
            worker.start()
            worker.join()
            return span

        with Fork(CLIENT, "weather-7", 1, tag_spans=False, set_env=False) as fork:
            # A web server's other worker thread, say: not the fork's.
            stray = started_on_a_thread()
            # A thread the agent started in a copy of the block's context.
            carried = started_on_a_thread(contextvars.copy_context())
        self.assertEqual(stray.attributes, {}, "another thread's span is not moved into the fork's run")
        self.assertEqual(carried.attributes["traza.fork.id"], fork.id)

        with Fork(CLIENT, "weather-7", 1, tag_spans=False, set_env=False, tag_threads=True) as every:
            anywhere = started_on_a_thread()
            with self.assertRaises(RuntimeError):
                Fork(CLIENT, "weather-7", 1, set_env=False, tag_threads=True).start()
        self.assertEqual(anywhere.attributes["traza.fork.id"], every.id)
        self.assertEqual(started_on_a_thread().attributes, {}, "nothing is tagged once it ends")

    def test_with_the_opentelemetry_sdk_the_processor_is_installed_and_tags(self):
        try:
            from opentelemetry import trace
            from opentelemetry.sdk.trace import TracerProvider
            from opentelemetry.sdk.trace.export import SimpleSpanProcessor
            from opentelemetry.sdk.trace.export.in_memory_span_exporter import InMemorySpanExporter
        except ImportError:
            self.skipTest("the OpenTelemetry SDK is not installed; the processor itself is tested above")
        if hasattr(trace.get_tracer_provider(), "add_span_processor"):
            self.skipTest("a tracer provider is already set in this process")
        exporter = InMemorySpanExporter()
        provider = TracerProvider()
        provider.add_span_processor(SimpleSpanProcessor(exporter))
        trace.set_tracer_provider(provider)
        with Fork(CLIENT, "weather-7", 1) as fork:
            self.assertTrue(fork.tagging)
            with trace.get_tracer("test").start_as_current_span("agent-step"):
                pass
        with trace.get_tracer("test").start_as_current_span("after"):
            pass
        spans = {span.name: dict(span.attributes) for span in exporter.get_finished_spans()}
        self.assertEqual(spans["agent-step"]["traza.fork.id"], fork.id)
        self.assertEqual(spans["agent-step"]["session.id"], fork.id)
        self.assertNotIn("traza.fork.id", spans["after"])


class ForkListing(unittest.TestCase):
    def test_a_recorded_fork_is_found_and_diffed_against_its_parent(self):
        with Fork(CLIENT, "weather-7", 1, tool_result="sunny", live=False, tag_spans=False) as fork:
            pass
        # What the forked run's instrumentation exports: the same agent, its
        # spans stamped by the fork.
        tags = fork.span_attributes()
        CLIENT.request("POST", "/v1/spans", body=weather_run(
            fork.run_id, "6df92f3577b34da6a3ce929d0e0e4738", at_ms=5_000, result="sunny", extra=tags))
        listing = CLIENT.forks("weather-7")
        mine = [item for item in listing if item.fork_id == fork.id]
        self.assertEqual(len(mine), 1, listing.raw)
        self.assertEqual(mine[0].run, fork.run_id)
        self.assertEqual(mine[0].fork_step, 1)
        self.assertEqual((mine[0].first_divergence.a, mine[0].first_divergence.b), (1, 1))
        self.assertEqual(mine[0].first_divergence.kind, "decision")
        self.assertFalse(mine[0].diverged_before_cut)
        self.assertEqual(listing.parent.steps, 3)
        self.assertEqual(fork.diff().first_divergence.b, 1)


class TapeTests(unittest.TestCase):
    def test_an_agent_on_the_recorded_path_passes(self):
        def agent(tape):
            base, key = os.environ["OPENAI_BASE_URL"], os.environ["OPENAI_API_KEY"]
            chat(base, key, {"model": "gpt-4o", "messages": messages()})
            chat(base, key, {"model": "gpt-4o", "messages": messages(tool_result=RAIN)})

        status = replay_test(CLIENT, "weather-7", agent)
        self.assertEqual((status.served, status.remaining), (2, 0))
        self.assertNotIn("TRAZA_REPLAY_ID", os.environ)

    def test_a_divergence_fails_naming_the_message_and_the_text(self):
        def agent(tape):
            chat(os.environ["OPENAI_BASE_URL"], os.environ["OPENAI_API_KEY"],
                 {"model": "gpt-4o", "messages": messages(user="What's the weather in Lyon?")})

        with self.assertRaises(TapeMismatch) as caught:
            replay_test(CLIENT, "weather-7", agent)
        message = str(caught.exception)
        self.assertIn("diverged from recorded run weather-7 at step 0", message)
        self.assertIn("message 1 (user): content differs at character 22", message)
        self.assertIn("recorded: \"What's the weather in Paris?\"", message)
        self.assertIn("got:      \"What's the weather in Lyon?\"", message)
        refusal = caught.exception.__cause__
        self.assertIsInstance(refusal, urllib.error.HTTPError, "the agent's own error is kept beneath")
        self.assertEqual(refusal.code, 409)

    def test_stopping_early_and_overrunning_both_fail(self):
        with TapeTest(CLIENT, "weather-7") as tape:
            chat(os.environ["OPENAI_BASE_URL"], os.environ["OPENAI_API_KEY"],
                 {"model": "gpt-4o", "messages": messages()})
        self.assertIn("stopped after 1 of the 2 model calls", tape.failure())
        with TapeTest(CLIENT, "weather-7", mode="loose") as tape:
            for _ in range(3):
                try:
                    chat(tape.replay.openai_base_url, "x", {"model": "gpt-4o", "messages": messages()})
                except urllib.error.HTTPError as error:
                    self.assertEqual(error.code, 410)
                    error.close()
        self.assertIn("1 model call(s) past the end", tape.failure())
        with self.assertRaises(TapeMismatch):
            tape.check()

    def test_describe_and_the_junit_report(self):
        self.assertIsNone(describe(ReplayStatus(served=2, remaining=0, exhausted_calls=0)))
        missing = describe(ReplayStatus(run="r", diverged=Divergence(step=2, difference={
            "message": 3, "role": "tool", "field": "missing", "recorded": "x", "got": None,
            "recorded_messages": 4, "got_messages": 3})))
        self.assertIn("message 3 (tool) is missing from the request", missing)
        report = junit_xml([
            GoldenResult("ok-run", None, True, None, None, 0.5, "rp_1", 0, "fine\n"),
            GoldenResult("bad-run", "acme", False, "divergence", "diverged\n  detail \x07bell", 1.25, "rp_2"),
            GoldenResult("dead-run", None, False, "error", "the replay could not be created", 0.01),
        ])
        root = ET.fromstring(report)
        suite = root.find("testsuite")
        self.assertEqual((suite.get("tests"), suite.get("failures"), suite.get("errors")), ("3", "1", "1"))
        cases = {case.get("name"): case for case in suite.findall("testcase")}
        self.assertIsNone(cases["ok-run"].find("failure"))
        failure = cases["bad-run"].find("failure")
        self.assertEqual(cases["bad-run"].get("classname"), "traza.golden.acme")
        self.assertEqual((failure.get("type"), failure.get("message")), ("divergence", "diverged"))
        self.assertIn("�bell", failure.text, "control characters cannot break the XML")
        self.assertIsNotNone(cases["dead-run"].find("error"))


def run_cli(*argv):
    out, err = io.StringIO(), io.StringIO()
    with redirect_stdout(out), redirect_stderr(err):
        code = cli_main(list(argv))
    return code, out.getvalue(), err.getvalue()


class GoldenRunsAndTheCommand(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        # A server of its own, so golden markers cannot leak into other tests.
        cls.server = Server()
        cls.addClassCleanup(cls.server.stop)
        cls.client = Client(cls.server.url)
        cls.client.request("POST", "/v1/spans", body=weather_run("golden-by-tag", "aaf92f3577b34da6a3ce929d0e0e4730",
                                                                 extra={"traza.golden": True}))
        cls.client.request("POST", "/v1/spans", body=weather_run("golden-by-note", "abf92f3577b34da6a3ce929d0e0e4731"))
        cls.agent = [sys.executable, str(Path(SCRATCH, "agent.py"))]

    def test_marking_and_listing_golden_runs(self):
        self.client.mark_golden("golden-by-note")
        listed = [(run.run, run.source) for run in self.client.goldens()]
        self.assertEqual(listed, [("golden-by-note", "annotation"), ("golden-by-tag", "attribute")])
        code, out, _ = run_cli("golden", "list", "--url", self.server.url)
        self.assertEqual(code, 0)
        self.assertIn("golden-by-tag\tattribute", out)

    def test_traza_test_replays_every_golden_run_and_reports_junit(self):
        self.client.mark_golden("golden-by-note")
        code, out, err = run_cli("test", "--url", self.server.url, "--", *self.agent, "{input}")
        self.assertEqual(code, 0, err)
        self.assertIn("PASS golden-by-note", err)
        root = ET.fromstring(out)
        cases = root.find("testsuite").findall("testcase")
        self.assertEqual(sorted(case.get("name") for case in cases), ["golden-by-note", "golden-by-tag"])
        self.assertTrue(all(case.find("failure") is None for case in cases))
        self.assertIn("It's raining in Paris", cases[0].find("system-out").text)

    def test_a_diverging_or_truncated_agent_fails_with_the_reason(self):
        report = Path(SCRATCH, "report.xml")
        code, _, err = run_cli("test", "--url", self.server.url, "--run", "golden-by-tag", "--junit", str(report),
                               "--", *self.agent, "What's the weather in Rome?")
        self.assertEqual(code, 1)
        self.assertIn("FAIL golden-by-tag", err)
        failure = ET.parse(report).getroot().find("testsuite/testcase/failure")
        self.assertEqual(failure.get("type"), "divergence")
        self.assertIn("content differs at character 22", failure.text)
        self.assertIn("What's the weather in Rome?", failure.text)
        code, out, _ = run_cli("test", "--url", self.server.url, "--run", "golden-by-tag",
                               "--", *self.agent, "{input}", "--stop-early")
        self.assertEqual(code, 1)
        failure = ET.fromstring(out).find("testsuite/testcase/failure")
        self.assertEqual(failure.get("type"), "incomplete")

    def test_a_partial_golden_listing_fails_unless_accepted(self):
        self.client.mark_golden("golden-by-note")
        self.assertEqual(build_parser().parse_args(["test"]).limit, 1000, "the server's maximum by default")
        code, out, err = run_cli("test", "--url", self.server.url, "--limit", "1", "--", *self.agent, "{input}")
        self.assertEqual(code, 1, err)
        self.assertIn("incomplete", err)
        self.assertIn("raise --limit", err)
        self.assertEqual(ET.fromstring(out).get("tests"), "1", "the runs listed are still tested")
        code, _, err = run_cli("test", "--url", self.server.url, "--limit", "1", "--allow-partial",
                               "--", *self.agent, "{input}")
        self.assertEqual(code, 0, err)

    def test_traza_forks_says_why_a_listing_is_incomplete(self):
        for index, trace in enumerate(("acf92f3577b34da6a3ce929d0e0e4732", "adf92f3577b34da6a3ce929d0e0e4733")):
            self.client.request("POST", "/v1/spans", body=weather_run(
                f"golden-by-tag-fork-{index}", trace, at_ms=10_000 * (index + 1),
                extra={"traza.fork.parent": "golden-by-tag", "traza.fork.step": 1}))
        code, _, err = run_cli("forks", "golden-by-tag", "--url", self.server.url, "--limit", "1")
        self.assertEqual(code, 0)
        self.assertIn("more forks exist than --limit 1", err)
        self.assertNotIn("smaller", err)

    def test_the_agent_gets_a_replay_key_never_an_operator_token(self):
        operator, reader, writer = "op-token", "acme-ro-token", "acme-rw-token"
        auth = Server(env={"TRAZA_TOKENS": f"admin:{operator},ro@acme:{reader},rw@acme:{writer}"})
        self.addCleanup(auth.stop)
        Client(auth.url, token=writer).request(
            "POST", "/v1/spans", body=weather_run("acme-golden", "aef92f3577b34da6a3ce929d0e0e4734",
                                                  extra={"traza.golden": True}))
        saved = {key: os.environ.get(key) for key in ("TRAZA_TOKEN", "TRAZA_REPLAY_KEY", "AGENT_KEY_FILE")}

        def restore():
            for key, value in saved.items():
                if value is None:
                    os.environ.pop(key, None)
                else:
                    os.environ[key] = value

        self.addCleanup(restore)
        keys = Path(SCRATCH, "agent-keys.txt")
        os.environ["AGENT_KEY_FILE"] = str(keys)
        os.environ.pop("TRAZA_REPLAY_KEY", None)
        os.environ["TRAZA_TOKEN"] = operator
        # The operator token lists every tenant's goldens, but is not handed
        # to an agent as its provider key.
        code, _, err = run_cli("test", "--url", auth.url, "--", *self.agent, "{input}")
        self.assertEqual(code, 2, err)
        self.assertIn("TRAZA_REPLAY_KEY", err)
        self.assertFalse(keys.exists(), "no agent ran")
        with self.assertRaises(ValueError):
            replay_key(Client(auth.url, token=operator), {})
        # A dedicated read-only key of the run's tenant is what the agent gets.
        os.environ["TRAZA_REPLAY_KEY"] = reader
        code, _, err = run_cli("test", "--url", auth.url, "--", *self.agent, "{input}")
        self.assertEqual(code, 0, err)
        self.assertEqual(keys.read_text().split(), [reader])
        # A bound token may be handed over as it is.
        os.environ.pop("TRAZA_REPLAY_KEY")
        os.environ["TRAZA_TOKEN"] = reader
        self.assertIsNone(replay_key(Client(auth.url, token=reader), {}))
        code, _, err = run_cli("test", "--url", auth.url, "--", *self.agent, "{input}")
        self.assertEqual(code, 0, err)
        self.assertEqual(keys.read_text().split(), [reader, reader])

    def test_an_empty_suite_is_not_a_pass_unless_asked(self):
        empty = Server()
        try:
            code, _, err = run_cli("test", "--url", empty.url, "--", *self.agent, "{input}")
            self.assertEqual(code, 1)
            self.assertIn("no golden runs", err)
            code, out, _ = run_cli("test", "--url", empty.url, "--allow-empty", "--", *self.agent, "{input}")
            self.assertEqual(code, 0)
            self.assertEqual(ET.fromstring(out).get("tests"), "0")
        finally:
            empty.stop()

    def test_traza_fork_runs_an_agent_on_a_branch(self):
        code, _, err = run_cli("fork", "golden-by-tag", "--url", self.server.url, "--at", "1",
                               "--tool-result", '"sunny"', "--no-live", "--", *self.agent, USER)
        self.assertEqual(code, 0, err)
        self.assertIn("served 2 recorded model call(s)", err)
        code, out, _ = run_cli("forks", "golden-by-tag", "--url", self.server.url)
        self.assertEqual(code, 0)


class PytestPlugin(unittest.TestCase):
    def test_marked_tests_replay_and_a_divergence_fails_with_the_difference(self):
        try:
            import pytest  # noqa: F401
        except ImportError:
            self.skipTest("pytest is not installed; the plugin's core (traza.testing) is tested above")
        directory = tempfile.mkdtemp(dir=SCRATCH)
        Path(directory, "test_agent.py").write_text(textwrap.dedent(
            '''
            import json, os, urllib.request
            import pytest

            SYSTEM = "You are a weather assistant."

            def chat(user, tool_result=None):
                messages = [{"role": "system", "content": SYSTEM}, {"role": "user", "content": user}]
                if tool_result is not None:
                    messages.append({"role": "assistant", "content": None, "tool_calls": [{"id": "call_w1",
                        "type": "function", "function": {"name": "get_weather", "arguments": "{\\"city\\":\\"Paris\\"}"}}]})
                    messages.append({"role": "tool", "tool_call_id": "call_w1", "content": tool_result})
                request = urllib.request.Request(os.environ["OPENAI_BASE_URL"] + "/chat/completions",
                    data=json.dumps({"model": "gpt-4o", "messages": messages}).encode(), method="POST",
                    headers={"Content-Type": "application/json",
                             "Authorization": "Bearer " + os.environ["OPENAI_API_KEY"]})
                with urllib.request.urlopen(request, timeout=10) as answer:
                    return json.loads(answer.read())

            @pytest.mark.traza_tape("weather-7")
            def test_follows_the_recording(traza_tape):
                chat("What's the weather in Paris?")
                result = traza_tape.tool_result("get_weather").result
                chat("What's the weather in Paris?", json.dumps(result))

            @pytest.mark.traza_tape("weather-7")
            def test_asks_about_another_city():
                chat("What's the weather in Oslo?")

            @pytest.mark.traza_tape("weather-7")
            def test_stops_early():
                chat("What's the weather in Paris?")

            @pytest.mark.traza_tape("weather-7")
            def test_skips_itself():
                pytest.skip("not today")

            @pytest.mark.traza_tape("weather-7")
            @pytest.mark.xfail(reason="known to ask about Oslo")
            def test_expected_to_fail():
                chat("What's the weather in Oslo?")

            def test_unmarked_tests_are_untouched():
                assert "TRAZA_REPLAY_ID" not in os.environ
            '''
        ))
        report = Path(directory, "junit.xml")
        env = {key: value for key, value in os.environ.items() if not key.startswith(("TRAZA_", "OPENAI_"))}
        env.update({"TRAZA_URL": SERVER.url, "PYTHONPATH": str(SRC)})
        finished = subprocess.run(
            [sys.executable, "-m", "pytest", "-p", "traza.pytest_plugin", "-p", "no:cacheprovider", "-q",
             f"--junitxml={report}", str(Path(directory, "test_agent.py"))],
            cwd=directory, env=env, capture_output=True, text=True, timeout=120,
        )
        output = finished.stdout + finished.stderr
        self.assertEqual(finished.returncode, 1, output)
        cases = {case.get("name"): case for case in ET.parse(report).getroot().iter("testcase")}
        self.assertIsNone(cases["test_follows_the_recording"].find("failure"), output)
        self.assertIsNone(cases["test_unmarked_tests_are_untouched"].find("failure"), output)
        diverged = cases["test_asks_about_another_city"].find("failure")
        self.assertIsNotNone(diverged, output)
        self.assertIn("traza tape: the agent diverged from recorded run weather-7 at step 0", diverged.text)
        self.assertIn("got:      \"What's the weather in Oslo?\"", diverged.text)
        stopped = cases["test_stops_early"].find("failure")
        self.assertIn("stopped after 1 of the 2 model calls", stopped.text)
        # A skip, and an expected failure that failed, stay what they were.
        for name in ("test_skips_itself", "test_expected_to_fail"):
            self.assertIsNone(cases[name].find("failure"), output)
            self.assertIsNotNone(cases[name].find("skipped"), output)


if __name__ == "__main__":
    unittest.main()
