"""Every client method against a real traza-server.

Two servers serve the module: an open one (no credential, so every caller is
an unbound admin) for the guard, tape, replay, diff, recall and ledger
surfaces, and one started with TRAZA_TOKENS for the auth, quota and
tenant-binding behaviour. Runs are seeded the way OpenLLMetry records them,
through ``Client.request`` — the client itself does not ingest.
"""

from __future__ import annotations

import asyncio
import json
import os
import subprocess
import sys
import time
import unittest
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

from _server import Server  # noqa: E402
from _stub import Stub, response  # noqa: E402

from traza import (  # noqa: E402
    AuthError,
    Client,
    Conflict,
    FuseTripped,
    Limits,
    NotFound,
    QuotaExceeded,
    TrazaError,
    otel_env,
)
from traza.models import _RawNumber  # noqa: E402  (the pack's verbatim-number type)

# 2026-01-01T00:00:00Z: old enough that every seeded run reads as finished
# (recall calls a run whose last span ended in the last 15 minutes unknown).
T0 = 1_767_225_600_000_000_000
MS = 1_000_000

ADMIN = "sdk-admin-token-0123456789abcdef"
ACME_RO = "sdk-acme-reader-0123456789abcdef"
ACME_RW = "sdk-acme-writer-0123456789abcdef"
BETA_RO = "sdk-beta-reader-0123456789abcdef"

SYSTEM = "You are a weather assistant."


def span(trace, span_id, parent, name, start_ms, end_ms, attributes, service="weather-agent", status="ok"):
    return {
        "trace_id": trace,
        "span_id": span_id,
        "parent_span_id": parent,
        "name": name,
        "service": service,
        "status": status,
        "start_time_ns": T0 + start_ms * MS,
        "end_time_ns": T0 + end_ms * MS,
        "attributes": attributes,
    }


def weather_run(session, trace, city, tenant=None):
    """OpenLLMetry's OpenAI instrumentation: a workflow, a model call asking
    for a tool, the tool, and the model call that answers."""
    user = f"What's the weather in {city}?"
    args = json.dumps({"city": city})
    result = json.dumps({"forecast": "rain", "celsius": 12})
    llm = {"gen_ai.system": "openai", "gen_ai.request.model": "gpt-4o", "gen_ai.response.model": "gpt-4o-2024-08-06"}
    spans = [
        span(trace, "wf", None, "weather_agent.workflow", 0, 1000,
             {"traceloop.span.kind": "workflow", "traceloop.entity.name": "weather_agent"}),
        span(trace, "llm-1", "wf", "openai.chat", 10, 400, {
            **llm,
            "gen_ai.usage.prompt_tokens": 82, "gen_ai.usage.completion_tokens": 17,
            "gen_ai.prompt.0.role": "system", "gen_ai.prompt.0.content": SYSTEM,
            "gen_ai.prompt.1.role": "user", "gen_ai.prompt.1.content": user,
            "gen_ai.completion.0.role": "assistant", "gen_ai.completion.0.finish_reason": "tool_calls",
            "gen_ai.completion.0.tool_calls.0.id": "call_w1",
            "gen_ai.completion.0.tool_calls.0.name": "get_weather",
            "gen_ai.completion.0.tool_calls.0.arguments": args,
        }),
        span(trace, "tool-1", "wf", "get_weather.tool", 410, 450, {
            "traceloop.span.kind": "tool", "traceloop.entity.name": "get_weather",
            "traceloop.entity.input": args, "traceloop.entity.output": result,
        }),
        span(trace, "llm-2", "wf", "openai.chat", 460, 900, {
            **llm,
            "gen_ai.usage.prompt_tokens": 120, "gen_ai.usage.completion_tokens": 14,
            "gen_ai.prompt.0.role": "system", "gen_ai.prompt.0.content": SYSTEM,
            "gen_ai.prompt.1.role": "user", "gen_ai.prompt.1.content": user,
            "gen_ai.prompt.2.role": "assistant",
            "gen_ai.prompt.2.tool_calls.0.id": "call_w1",
            "gen_ai.prompt.2.tool_calls.0.name": "get_weather",
            "gen_ai.prompt.2.tool_calls.0.arguments": args,
            "gen_ai.prompt.3.role": "tool", "gen_ai.prompt.3.tool_call_id": "call_w1",
            "gen_ai.prompt.3.content": result,
            "gen_ai.completion.0.role": "assistant", "gen_ai.completion.0.finish_reason": "stop",
            "gen_ai.completion.0.content": f"It's raining in {city}, 12°C. Take an umbrella.",
        }),
    ]
    for item in spans:
        item["attributes"]["session.id"] = session
        if tenant is not None:
            item["$tenant"] = tenant
    return spans


def tool_steps(session, trace, count, tool="search_orders", service="loop-agent", tenant=None):
    """``count`` identical tool calls: what a looping agent records."""
    spans = []
    for i in range(count):
        attributes = {
            "session.id": session,
            "gen_ai.operation.name": "execute_tool",
            "gen_ai.tool.name": tool,
            "gen_ai.tool.call.arguments": json.dumps({"order": 42}),
        }
        item = span(trace, f"t{i}", None, f"{tool}.tool", i * 10, i * 10 + 5, attributes, service=service)
        if tenant is not None:
            item["$tenant"] = tenant
        spans.append(item)
    return spans


def openai_request(city, second=False):
    """The chat-completions body the OpenAI SDK sends for the weather agent."""
    messages = [{"role": "system", "content": SYSTEM}, {"role": "user", "content": f"What's the weather in {city}?"}]
    if second:
        messages.append({"role": "assistant", "content": None, "tool_calls": [{
            "id": "call_w1", "type": "function",
            "function": {"name": "get_weather", "arguments": json.dumps({"city": city}, separators=(",", ":"))},
        }]})
        messages.append({"role": "tool", "tool_call_id": "call_w1",
                         "content": json.dumps({"forecast": "rain", "celsius": 12})})
    return {"model": "gpt-4o", "messages": messages}


def dump_reversed(value):
    """JSON with every object's keys reversed and spaces added, numbers verbatim."""
    if isinstance(value, dict):
        return "{ " + ", ".join(json.dumps(k) + ": " + dump_reversed(value[k]) for k in reversed(list(value))) + " }"
    if isinstance(value, list):
        return "[ " + ", ".join(dump_reversed(item) for item in value) + " ]"
    if isinstance(value, _RawNumber):
        return str(value)
    return json.dumps(value, ensure_ascii=False)


def post_json(url, body, headers):
    """A provider SDK's request, made with nothing but urllib."""
    request = urllib.request.Request(
        url, data=json.dumps(body).encode(), method="POST",
        headers={"Content-Type": "application/json", **headers},
    )
    with urllib.request.urlopen(request, timeout=10) as response:
        return response.status, json.loads(response.read())


OPEN = None
AUTH = None


def setUpModule():
    global OPEN, AUTH
    # Each server is handed to addModuleCleanup the moment it runs, not left
    # to a tearDownModule: unittest skips tearDownModule when setUpModule
    # raises, so the second server failing to start, or a seed request below
    # being refused, would leave processes and stores behind. Module cleanups
    # run either way.
    OPEN = Server()
    unittest.addModuleCleanup(OPEN.stop)
    AUTH = Server(env={"TRAZA_TOKENS": f"admin:{ADMIN},ro@acme:{ACME_RO},rw@acme:{ACME_RW},ro@beta:{BETA_RO}"})
    unittest.addModuleCleanup(AUTH.stop)
    seed = Client(OPEN.url)
    seed.request("POST", "/v1/spans", body=weather_run("weather-7", "4bf92f3577b34da6a3ce929d0e0e4736", "Paris"))
    lyon = weather_run("weather-8", "5cf92f3577b34da6a3ce929d0e0e4737", "Lyon")
    # A metered cost the server prints as 0.00001 and Python would re-print
    # as 1e-05: the evidence digest check must keep the server's text, or it
    # would hash different bytes.
    lyon[1]["attributes"]["llm.cost_usd"] = 0.00001
    seed.request("POST", "/v1/spans", body=lyon)


class IdentityAndUsage(unittest.TestCase):
    def test_whoami_on_an_open_server(self):
        me = Client(OPEN.url).whoami()
        self.assertEqual(me.auth, "open")
        self.assertEqual(me.scope, "admin")
        self.assertTrue(me.operator)
        self.assertIsNone(me.tenant)
        self.assertIsNone(me.credential)

    def test_whoami_names_a_bound_credential(self):
        me = Client(AUTH.url, token=ACME_RW).whoami()
        self.assertEqual((me.auth, me.scope, me.tenant, me.operator), ("token", "rw", "acme", False))
        self.assertTrue(me.credential.id.startswith("cr_"))

    def test_usage_counts_what_was_sent(self):
        report = Client(OPEN.url, tenant="").usage()
        self.assertIsInstance(report.as_of_ns, int)
        [row] = report.tenants
        self.assertEqual(row.tenant, "")
        self.assertGreaterEqual(row.lifetime.spans, 8)
        self.assertGreater(row.lifetime.bytes, 0)
        self.assertEqual(row.state, "ok")
        self.assertRegex(row.month.window, r"^\d{4}-\d{2}$")
        self.assertIsNone(row.limits.monthly_spans)


class Fuses(unittest.TestCase):
    def test_check_answers_a_hypothetical_stop_without_tripping(self):
        client = Client(OPEN.url, tenant="checks")
        client.put_policy("default", Limits(max_cost_usd=1.0), action="stop")
        fresh = client.check("fresh-run")
        self.assertEqual(fresh.verdict, "continue")
        self.assertEqual(fresh.policy, "default")
        self.assertEqual(fresh.limits.max_cost_usd, 1.0)
        over = client.check("fresh-run", est_cost_usd=1.5, est_tokens=10, tool="search")
        self.assertTrue(over.stopped)
        self.assertIn("would be exceeded by the next step", over.reasons[0])
        self.assertIsNone(over.tripped_by, "a hypothetical stop is not a recorded one")
        self.assertEqual(client.run_verdict("fresh-run").verdict, "continue", "the check changed nothing")

    def test_guard_raises_fuse_tripped_and_the_body_never_runs(self):
        client = Client(OPEN.url, tenant="guarded")
        client.put_policy("loop", Limits(max_repeats=3), service="loop-agent", action="stop")
        ack = client.request("POST", "/v1/spans", body=tool_steps("loop-1", "a1" * 16, 3, tenant="guarded"))
        self.assertEqual(ack["fuses"][0]["verdict"], "stop", ack)

        ran = []
        with self.assertRaises(FuseTripped) as caught:
            with client.guard("loop-1", tool="search_orders"):
                ran.append("body")
        self.assertEqual(ran, [])
        verdict = caught.exception.verdict
        self.assertEqual(verdict.verdict, "stop")
        self.assertEqual(verdict.tripped_by, "policy")
        self.assertEqual(verdict.usage.top_repeat.count, 3)
        self.assertTrue(verdict.usage.top_repeat.fingerprint.startswith("tool:search_orders#"))
        self.assertIn("max_repeats 3 reached", str(caught.exception))
        self.assertNotIsInstance(caught.exception, TrazaError)

        # A fresh run of the same tenant passes the same guard.
        with client.guard("loop-2") as guard:
            ran.append("fresh")
        self.assertEqual(ran, ["fresh"])
        self.assertEqual(guard.verdict.verdict, "continue")

    def test_guard_as_a_decorator_names_the_run_from_the_arguments(self):
        client = Client(OPEN.url, tenant="guarded")
        client.trip("node-stopped", reason="sdk decorator test")

        @client.guard(lambda state: state["run_id"])
        def agent_node(state):
            return state["n"] + 1

        self.assertEqual(agent_node({"run_id": "node-ok", "n": 1}), 2)
        with self.assertRaises(FuseTripped) as caught:
            agent_node({"run_id": "node-stopped", "n": 1})
        self.assertEqual(caught.exception.verdict.tripped_by, "manual")

        @client.guard(lambda state: state["run_id"])
        async def async_node(state):
            return "ran"

        self.assertEqual(asyncio.run(async_node({"run_id": "node-ok"})), "ran")
        with self.assertRaises(FuseTripped):
            asyncio.run(async_node({"run_id": "node-stopped"}))

        async def with_block():
            async with client.guard("node-stopped"):
                return "ran"

        with self.assertRaises(FuseTripped):
            asyncio.run(with_block())

    def test_guard_fails_closed_unless_told_to_fail_open(self):
        unreachable = Client("http://127.0.0.1:1", timeout=2)
        with self.assertRaises(TrazaError) as caught:
            with unreachable.guard("any"):
                pass
        self.assertIsNone(caught.exception.status)
        ran = []
        with unreachable.guard("any", fail_open=True) as guard:
            ran.append("body")
        self.assertEqual(ran, ["body"])
        self.assertIsNone(guard.verdict)
        self.assertIsInstance(guard.error, TrazaError)

    def test_trip_reset_runs_and_the_event_feed(self):
        client = Client(OPEN.url, tenant="ops")
        start = client.events()
        list(start)  # drain whatever the feed already holds for this tenant
        cursor = start.cursor
        self.assertIsNotNone(cursor)

        tripped = client.trip("sess-9c21", reason="customer escalated")
        self.assertEqual((tripped.verdict, tripped.tripped_by), ("stop", "manual"))
        self.assertEqual(tripped.reasons, ["manually tripped: customer escalated"])
        self.assertIsInstance(tripped.tripped_at_ns, int)
        self.assertIn("sess-9c21", [run.run for run in client.runs("stop")])

        reset = client.reset("sess-9c21", grant={"max_steps": 10})
        self.assertEqual(reset.verdict, "continue")
        self.assertEqual(reset.grant.max_steps, 10)
        self.assertEqual(reset.limits.max_steps, 10)
        self.assertNotIn("sess-9c21", [run.run for run in client.runs("stop", limit=50)])

        stream = client.events(after=cursor)
        kinds = [(event.kind, event.run) for event in stream]
        self.assertEqual(kinds, [("manual_stop", "sess-9c21"), ("reset", "sess-9c21")])
        self.assertFalse(stream.gap)

        # A consumer that stops after one event resumes at the next one.
        partial = client.events(after=cursor)
        for event in partial:
            self.assertEqual(event.kind, "manual_stop")
            break
        self.assertEqual([event.kind for event in client.events(after=partial.cursor)], ["reset"])

        # Resuming from the stream's cursor sees only what came after it.
        client.trip("sess-9c21")
        resumed = client.events(after=stream.cursor)
        self.assertEqual([event.kind for event in resumed], ["manual_stop"])

        # A long-poll with nothing new waits, then ends.
        quiet = client.events(after=resumed.cursor, wait_ms=300)
        began = time.monotonic()
        self.assertEqual(list(quiet), [])
        self.assertGreaterEqual(time.monotonic() - began, 0.25)

    def test_a_following_stream_long_polls_unless_told_how_long_to_wait(self):
        # A stub that answers every read at once with an empty page, as a
        # quiet feed would after its wait, and refuses the fifth so the
        # endless stream ends. Every read a follower makes must ask the server
        # to hold it open: with wait_ms=0 it would poll as fast as the network
        # allows (thousands of requests a second against a real server).
        def answer(request):
            if len(stub.requests) > 4:
                return response(500, b'{"error":"enough"}')
            return response(200, b'{"events":[],"next":"1.0"}', {"Content-Type": "application/json"})

        with Stub(answer) as stub:
            with self.assertRaises(TrazaError):
                for _ in Client(stub.url).events(follow=True):
                    self.fail("the stub sends no events")
            self.assertEqual([r.query["wait_ms"] for r in stub.requests], ["25000"] * 5)
            self.assertEqual([r.query.get("after") for r in stub.requests], [None] + ["1.0"] * 4)

        # A wait the caller names is kept; one that cannot wait is refused,
        # since following with it would be the same busy loop.
        with Stub(lambda request: response(500, b"{}")) as stub:
            with self.assertRaises(TrazaError):
                list(Client(stub.url).events(follow=True, wait_ms=300))
            self.assertEqual(stub.requests[0].query["wait_ms"], "300")
        with self.assertRaises(ValueError):
            Client(OPEN.url).events(follow=True, wait_ms=0)
        # Without follow a read drains what is there and ends, so no wait.
        with Stub(lambda request: response(200, b'{"events":[]}')) as stub:
            self.assertEqual(list(Client(stub.url).events()), [])
            self.assertEqual(stub.requests[0].query["wait_ms"], "0")

    def test_a_run_id_is_one_path_segment_whatever_it_contains(self):
        # Run ids come from telemetry: `/`, `?`, `#`, `%` and spaces must all
        # arrive as one percent-encoded segment.
        run = "team/a?x=1#frag %41"
        client = Client(OPEN.url)
        client.request("POST", "/v1/spans", body=tool_steps(run, "b2" * 16, 1, service="slash-agent"))
        verdict = client.run_verdict(run)
        self.assertEqual(verdict.run, run)
        self.assertEqual(verdict.usage.tool_calls, 1)
        self.assertEqual(client.trip(run).run, run)
        self.assertEqual(client.tape(run).steps[0].tool, "search_orders")

    def test_policies_create_replace_list_and_delete(self):
        client = Client(OPEN.url, tenant="policies")
        created = client.put_policy(
            "support", {"max_cost_usd": 2.0, "max_steps": 40, "max_repeats": 4}, service="support-agent", action="stop"
        )
        self.assertTrue(created.id.startswith("fp_"))
        self.assertEqual((created.tenant, created.match.service, created.action), ("policies", "support-agent", "stop"))
        replaced = client.put_policy("support", Limits(max_steps=80), service="support-agent", action="stop")
        self.assertEqual(replaced.id, created.id, "same tenant and name replaces in place")
        self.assertEqual(replaced.limits.max_steps, 80)
        self.assertIsNone(replaced.limits.max_cost_usd, "limits are replaced whole, not merged")

        listing = client.policies()
        self.assertEqual([p.name for p in listing.policies], ["support"])
        self.assertEqual(listing.global_policy.name, "global")
        self.assertEqual(listing.global_policy.limits.max_repeats, 12)

        deleted = client.delete_policy(created.id)
        self.assertEqual(deleted.id, created.id)
        self.assertEqual(client.policies().policies, [])
        with self.assertRaises(NotFound):
            client.delete_policy(created.id)


class TapesAndReplays(unittest.TestCase):
    def test_tape(self):
        tape = Client(OPEN.url).tape("weather-7")
        self.assertEqual([step.kind for step in tape.steps], ["llm", "tool", "llm"])
        first = tape.steps[0]
        self.assertEqual(first.model, "gpt-4o-2024-08-06")
        self.assertEqual(first.request[1], {"role": "user", "content": "What's the weather in Paris?"})
        self.assertEqual(first.response[0]["tool_calls"][0]["name"], "get_weather")
        self.assertEqual((first.tokens.input, first.tokens.output, first.tokens.cache_read), (82, 17, None))
        self.assertEqual(len(first.request_digest), 64)
        self.assertEqual(tape.steps[1].tool, "get_weather")
        self.assertEqual(tape.steps[1].response, {"celsius": 12, "forecast": "rain"})
        self.assertFalse(tape.truncated)
        self.assertEqual(tape.spans_examined, 4)
        self.assertTrue(Client(OPEN.url).tape("weather-7", max_steps=1).truncated)
        with self.assertRaises(NotFound):
            Client(OPEN.url).tape("no-such-run")

    def test_a_replay_consumed_by_a_plain_http_client_on_the_openai_route(self):
        client = Client(OPEN.url)
        replay = client.replay("weather-7")
        self.assertTrue(replay.id.startswith("rp_"))
        self.assertEqual((replay.mode, replay.steps, replay.llm_steps), ("strict", 3, 2))
        env = replay.env()
        self.assertEqual(env["OPENAI_BASE_URL"], f"{OPEN.url}/v1/replays/{replay.id}/openai/v1")
        self.assertEqual(env["ANTHROPIC_BASE_URL"], f"{OPEN.url}/v1/replays/{replay.id}/anthropic")
        self.assertEqual(env["OPENAI_BASE_URL"], replay.openai_base_url)
        self.assertTrue(env["OPENAI_API_KEY"], "an SDK refuses to start without a key")

        auth = {"Authorization": "Bearer " + env["OPENAI_API_KEY"]}
        status, first = post_json(env["OPENAI_BASE_URL"] + "/chat/completions", openai_request("Paris"), auth)
        self.assertEqual(status, 200)
        self.assertEqual(first["object"], "chat.completion")
        call = first["choices"][0]["message"]["tool_calls"][0]
        self.assertEqual((call["id"], call["function"]["name"]), ("call_w1", "get_weather"))
        self.assertEqual(json.loads(call["function"]["arguments"]), {"city": "Paris"})

        # The harness takes the tool's result from the tape instead of running it.
        tool = replay.next_tool("get_weather")
        self.assertEqual((tool.step, tool.arguments, tool.result), (1, {"city": "Paris"}, {"celsius": 12, "forecast": "rain"}))
        self.assertIsNone(replay.next_tool(), "no recorded tool step remains")

        status, second = post_json(env["OPENAI_BASE_URL"] + "/chat/completions", openai_request("Paris", second=True), auth)
        self.assertEqual(second["choices"][0]["message"]["content"], "It's raining in Paris, 12°C. Take an umbrella.")
        self.assertEqual(second["usage"]["prompt_tokens"], 120)

        state = replay.status()
        self.assertEqual((state.served, state.remaining, state.diverged), (2, 0, None))
        self.assertEqual([(c.step, c.matched) for c in state.calls], [(0, True), (2, True)])

        with self.assertRaises(urllib.error.HTTPError) as caught:
            post_json(env["OPENAI_BASE_URL"] + "/chat/completions", openai_request("Paris"), auth)
        self.assertEqual(caught.exception.code, 410)

    def test_a_strict_divergence_maps_to_conflict(self):
        client = Client(OPEN.url)
        replay = client.replay("weather-7")
        path = replay.base_urls["openai"] + "/chat/completions"
        with self.assertRaises(Conflict) as caught:
            client.request("POST", path, body=openai_request("Lyon"))
        error = caught.exception
        self.assertEqual((error.status, error.type), (409, "replay_divergence"))
        self.assertFalse(error.retryable, "x-should-retry: false")
        self.assertEqual(replay.status().diverged.step, 0)

        loose = client.replay("weather-7", mode="loose")
        answer = client.request("POST", loose.base_urls["openai"] + "/chat/completions", body=openai_request("Lyon"))
        self.assertEqual(answer["choices"][0]["finish_reason"], "tool_calls")
        self.assertEqual([c.matched for c in loose.status().calls], [False])

    def test_unknown_replay_is_not_found(self):
        with self.assertRaises(NotFound):
            Client(OPEN.url)._replay_status("rp_00000000000000000000000000000000")

    def test_a_bound_reader_replays_through_the_anthropic_route(self):
        Client(AUTH.url, token=ACME_RW).request("POST", "/v1/spans", body=weather_run("acme-weather", "c3" * 16, "Oslo"))
        replay = Client(AUTH.url, token=ACME_RO).replay("acme-weather")
        env = replay.env()
        self.assertEqual(env["ANTHROPIC_API_KEY"], ACME_RO)
        status, message = post_json(
            env["ANTHROPIC_BASE_URL"] + "/v1/messages",
            {"model": "claude-x", "max_tokens": 256, "system": SYSTEM,
             "messages": [{"role": "user", "content": "What's the weather in Oslo?"}]},
            {"x-api-key": env["ANTHROPIC_API_KEY"], "anthropic-version": "2023-06-01"},
        )
        self.assertEqual(status, 200)
        self.assertEqual(message["stop_reason"], "tool_use")
        self.assertEqual(message["content"][0]["input"], {"city": "Oslo"})
        with self.assertRaises(NotFound, msg="another tenant's replay is a 404"):
            Client(AUTH.url, token=BETA_RO)._replay_status(replay.id)

    def test_env_can_hand_the_agent_its_own_key(self):
        # The operator creates the replay under the run's tenant, and the
        # agent process gets a bound reader's key instead of the operator's:
        # every library in that process can read what env() exports.
        Client(AUTH.url, token=ACME_RW).request("POST", "/v1/spans", body=weather_run("acme-env", "c4" * 16, "Bergen"))
        replay = Client(AUTH.url, token=ADMIN, tenant="acme").replay("acme-env")
        env = replay.env(api_key=ACME_RO)
        self.assertEqual((env["OPENAI_API_KEY"], env["ANTHROPIC_API_KEY"]), (ACME_RO, ACME_RO))
        self.assertNotIn(ADMIN, env.values())
        self.assertEqual(env["OPENAI_BASE_URL"], replay.openai_base_url)
        status, answer = post_json(
            env["OPENAI_BASE_URL"] + "/chat/completions", openai_request("Bergen"),
            {"Authorization": "Bearer " + env["OPENAI_API_KEY"]},
        )
        self.assertEqual(status, 200)
        self.assertEqual(answer["choices"][0]["finish_reason"], "tool_calls")
        # Without a key the client's own credential stands in, as before.
        self.assertEqual(replay.env()["OPENAI_API_KEY"], ADMIN)


class DiffAndRecall(unittest.TestCase):
    def test_diff_names_the_first_divergence(self):
        diff = Client(OPEN.url).diff("weather-7", "weather-8")
        self.assertEqual((diff.a.run, diff.b.run), ("weather-7", "weather-8"))
        self.assertEqual(diff.a.outcome, "success")
        self.assertEqual([(s.op, s.why) for s in diff.aligned], [("changed", "input")] * 3)
        self.assertEqual((diff.first_divergence.a, diff.first_divergence.b, diff.first_divergence.kind), (0, 0, "input"))
        self.assertEqual(diff.delta.steps, 0)
        same = Client(OPEN.url).diff("weather-7", "weather-7")
        self.assertIsNone(same.first_divergence)
        self.assertEqual({s.op for s in same.aligned}, {"same"})
        with self.assertRaises(NotFound):
            Client(OPEN.url).diff("weather-7", "no-such-run")

    def test_recall_ranks_earlier_runs(self):
        found = Client(OPEN.url).recall("weather Paris")
        self.assertEqual([run.run for run in found], ["weather-7"])
        run = found.runs[0]
        self.assertEqual((run.outcome, run.outcome_source), ("success", "derived"))
        self.assertEqual(run.skeleton, ["llm:gpt-4o-2024-08-06", "tool:get_weather", "llm:gpt-4o-2024-08-06"])
        self.assertGreaterEqual(run.score, 1)
        self.assertEqual(len(Client(OPEN.url).recall("weather", limit=1)), 1)
        self.assertEqual(len(Client(OPEN.url).recall("weather Paris", outcome="failure")), 0)


class Ledger(unittest.TestCase):
    def test_ledger_head(self):
        head = Client(OPEN.url).ledger_head()
        self.assertGreaterEqual(head.seq, 1)
        self.assertRegex(head.head, r"^[0-9a-f]{64}$")
        self.assertRegex(head.store_id, r"^[0-9a-f]{32}$")
        self.assertIsInstance(head.at_ns, int)

    def test_evidence_pack_and_its_digest(self):
        client = Client(OPEN.url)
        client.request("POST", "/v1/checkpoint")
        pack = client.evidence()
        self.assertEqual(pack.format, "traza-evidence/1")
        # The engine's version, whatever this package's is: the two are kept
        # in step by scripts/check-sdk-metadata.py at the bump, not here.
        self.assertRegex(pack.engine_version, r"^\d+\.\d+\.\d+")
        self.assertIsNone(pack.tenant)
        self.assertGreaterEqual(pack.activity.spans, 8)
        self.assertTrue(pack.ledger["verification"]["intact"])
        self.assertIn('"cost_usd":0.00001,', pack._text, "the number Python would re-print differently")
        self.assertTrue(pack.verify_digest())

        # The check is over the bytes received: change one and it fails.
        tampered = client.evidence()
        tampered._text = tampered._text.replace('"format":"traza-evidence/1"', '"format":"traza-evidence/2"')
        self.assertFalse(tampered.verify_digest())

        # A pack saved in another key order, with whitespace, still verifies:
        # the digest is over the canonical form, not the bytes as stored.
        reordered = client.evidence()
        reordered._text = dump_reversed(json.loads(reordered._text, parse_float=_RawNumber))
        self.assertTrue(reordered.verify_digest())

        window = client.evidence(since=0, until=1)
        self.assertEqual((window.window.since_ns, window.window.until_ns), (0, 1))
        self.assertEqual(window.activity.spans, 0)
        self.assertTrue(window.verify_digest())

        bound = Client(AUTH.url, token=ACME_RO).evidence()
        self.assertEqual(bound.tenant, "acme")
        self.assertEqual(bound.usage.tenant, "acme")
        self.assertTrue(bound.verify_digest())


class AuthAndQuotas(unittest.TestCase):
    def test_status_codes_map_to_exceptions(self):
        with self.assertRaises(AuthError) as caught:
            Client(AUTH.url).whoami()
        self.assertEqual(caught.exception.status, 401)
        with self.assertRaises(AuthError) as caught:
            Client(AUTH.url, token=ACME_RO).trip("r1")
        self.assertEqual((caught.exception.status, caught.exception.required), (403, "rw"))
        with self.assertRaises(AuthError) as caught:
            Client(AUTH.url, token=ACME_RO, tenant="beta").run_verdict("r1")
        self.assertEqual(caught.exception.message, "this credential is bound to a different tenant")
        # A ro token may ask the gate: the check is a read that carries a body.
        self.assertEqual(Client(AUTH.url, token=ACME_RO).check("r1").tenant, "acme")

    def test_a_quota_refusal_carries_retry_after_and_the_quota(self):
        admin = Client(AUTH.url, token=ADMIN)
        admin.request("POST", "/v1/admin/tenants/quota-co/limits", body={"monthly_spans": 2})
        writer = Client(AUTH.url, token=ADMIN)
        with self.assertRaises(QuotaExceeded) as caught:
            writer.request("POST", "/v1/spans", body=tool_steps("q1", "d4" * 16, 3, tenant="quota-co"))
        error = caught.exception
        self.assertEqual(error.status, 429)
        self.assertGreater(error.retry_after, 0)
        self.assertEqual(error.quota["dimension"], "monthly_spans")
        self.assertEqual(error.quota["limit"], 2)
        [row] = Client(AUTH.url, token=ADMIN, tenant="quota-co").usage().tenants
        self.assertEqual((row.lifetime.rejected, row.lifetime.spans, row.limits.monthly_spans), (3, 0, 2))

    def test_a_full_replay_share_is_a_429_without_a_quota(self):
        Client(AUTH.url, token=ADMIN).request(
            "POST", "/v1/spans", body=weather_run("beta-weather", "e5" * 16, "Rome", tenant="beta")
        )
        reader = Client(AUTH.url, token=BETA_RO)
        for _ in range(64):
            reader.replay("beta-weather")
        with self.assertRaises(QuotaExceeded) as caught:
            reader.replay("beta-weather")
        self.assertGreater(caught.exception.retry_after, 0)
        self.assertIsNone(caught.exception.quota)


class OtelEnv(unittest.TestCase):
    def test_variables(self):
        self.assertEqual(
            otel_env("http://traza.internal:8080/", token="trz_abc", tenant="acme"),
            {
                "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT": "http://traza.internal:8080/v1/traces",
                "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL": "http/protobuf",
                "OTEL_EXPORTER_OTLP_TRACES_HEADERS": "Authorization=Bearer%20trz_abc",
                "OTEL_RESOURCE_ATTRIBUTES": "traza.tenant=acme",
            },
        )
        self.assertEqual(
            sorted(otel_env("http://127.0.0.1:8080")),
            ["OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL"],
        )

    def test_the_endpoint_headers_and_tenant_reach_the_server(self):
        # An exporter decodes the headers variable (W3C baggage form) and
        # posts to the endpoint as given. OTLP/JSON stands in for protobuf
        # here, which the stdlib cannot encode; the route takes both.
        env = otel_env(AUTH.url, token=ADMIN, tenant="otel-co")
        name, _, value = env["OTEL_EXPORTER_OTLP_TRACES_HEADERS"].partition("=")
        resource_key, _, resource_value = env["OTEL_RESOURCE_ATTRIBUTES"].partition("=")
        export = {"resourceSpans": [{
            "resource": {"attributes": [
                {"key": "service.name", "value": {"stringValue": "otel-sdk-test"}},
                {"key": resource_key, "value": {"stringValue": resource_value}},
            ]},
            "scopeSpans": [{"spans": [{
                "traceId": "f6" * 16, "spanId": "f6" * 8, "name": "otel-span",
                "startTimeUnixNano": str(T0), "endTimeUnixNano": str(T0 + MS),
            }]}],
        }]}
        status, body = post_json(
            env["OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"], export, {urllib.parse.unquote(name): urllib.parse.unquote(value)}
        )
        self.assertEqual((status, body), (200, {"partialSuccess": {}}))
        [row] = Client(AUTH.url, token=ADMIN, tenant="otel-co").usage().tenants
        self.assertEqual(row.lifetime.spans, 1)


class Transport(unittest.TestCase):
    def test_a_redirect_is_refused_and_the_token_goes_nowhere(self):
        # Something in front of Traza (a proxy upgrading the scheme or
        # canonicalising the host) redirects to another origin. urllib would
        # follow with every header the request carried, Authorization too.
        with Stub(lambda request: response(200, b"{}")) as elsewhere:
            location = f"http://localhost:{elsewhere.port}/steal"
            for status in (301, 302, 303, 307, 308):
                with Stub(lambda request: response(status, b"moved", {"Location": location})) as front:
                    client = Client(front.url, token="trz_secret_token_0123456789")
                    for call in (client.ledger_head, lambda: client.check("r1")):
                        with self.assertRaises(TrazaError) as caught:
                            call()
                        self.assertEqual(caught.exception.status, status)
                        self.assertIn(location, str(caught.exception))
                    self.assertEqual(len(front.requests), 2)
            self.assertEqual(elsewhere.requests, [], "nothing followed the redirect")

    def test_a_path_not_starting_with_a_slash_is_refused_before_the_token_is_attached(self):
        # The path is appended to the client's URL, so one that does not start
        # with "/" continues the host instead of starting the path:
        # "http://traza.example" and ".attacker.example/v1/whoami" make a
        # request to traza.example.attacker.example, token and all, with no
        # redirect involved. A caller forwarding a route it was handed would
        # give its credential away. The stub stands in as an HTTP proxy, so it
        # receives every request whatever host it names, without any name
        # having to resolve; the proxy settings stay in force for the whole
        # test because urllib asks whether to bypass the proxy on every call.
        token = "trz_secret_token_0123456789"
        with Stub(lambda request: response(200, b"{}")) as proxy:
            with mock.patch.dict(os.environ, {"http_proxy": proxy.url, "no_proxy": ""}):
                client = Client("http://traza.example", token=token)
                # What a permitted request looks like through the proxy, so
                # its silence below means nothing was sent.
                self.assertEqual(client.request("GET", "/v1/whoami"), {})
                [allowed] = proxy.requests
                self.assertEqual(allowed.target, "http://traza.example/v1/whoami")
                self.assertEqual(allowed.headers["authorization"], "Bearer " + token)
                for path in (
                    ".attacker.example/v1/whoami",
                    "@attacker.example/v1/whoami",
                    ":8443/v1/whoami",
                    "v1/whoami",
                    "",
                    "http://attacker.example/v1/whoami",
                ):
                    with self.subTest(path), self.assertRaises(ValueError):
                        client.request("GET", path)
                self.assertEqual(
                    [request.target for request in proxy.requests[1:]], [], "a refused path sent nothing anywhere"
                )

    def test_an_unreadable_answer_is_a_traza_error_and_fail_open_covers_it(self):
        # A reverse proxy's sign-in page, a captive portal, a JSON value that
        # is not the object the route answers, a check with no verdict, a body
        # cut short, a status line that is not HTTP: each is an answer the
        # guard cannot act on, so it must fail closed by default and open
        # when asked, exactly like an unreachable server.
        json_type = {"Content-Type": "application/json"}
        answers = {
            "html": response(200, b"<html>Sign in</html>", {"Content-Type": "text/html"}),
            "array": response(200, b"[]", json_type),
            "empty": response(200),
            "no verdict": response(200, b'{"run":"r1"}', json_type),
            "unknown verdict": response(200, b'{"run":"r1","verdict":"maybe"}', json_type),
            "not utf-8": response(200, b'{"verdict":"\xff"}', json_type),
            "truncated": b'HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{"verd',
            "garbage status": b"HELLO\r\n\r\n",
        }
        for name, raw in answers.items():
            with self.subTest(name), Stub(lambda request, raw=raw: raw) as stub:
                client = Client(stub.url, timeout=5)
                with self.assertRaises(TrazaError):
                    with client.guard("r1"):
                        self.fail("a fail-closed guard ran its body")
                ran = []
                with client.guard("r1", fail_open=True) as guard:
                    ran.append("body")
                self.assertEqual(ran, ["body"])
                self.assertIsNone(guard.verdict)
                self.assertIsInstance(guard.error, TrazaError)
        # When the status is known it is kept, with the text that arrived.
        with Stub(lambda request: answers["html"]) as stub:
            with self.assertRaises(TrazaError) as caught:
                Client(stub.url).ledger_head()
            self.assertEqual(caught.exception.status, 200)
            self.assertEqual(caught.exception.body, "<html>Sign in</html>")
            with self.assertRaises(TrazaError):
                Client(stub.url).request("GET", "/v1/anything")
            with self.assertRaises(TrazaError):
                Client(stub.url).evidence()


class Configuration(unittest.TestCase):
    def test_from_env_and_repr(self):
        client = Client.from_env({"TRAZA_URL": "http://h:1/", "TRAZA_TOKEN": "secret", "TRAZA_TENANT": "acme"})
        self.assertEqual((client.url, client.token, client.tenant), ("http://h:1", "secret", "acme"))
        self.assertNotIn("secret", repr(client))
        with self.assertRaises(ValueError):
            Client("127.0.0.1:8080")

    def test_an_empty_tenant_variable_is_unset(self):
        # As the shell hook reads it: `TRAZA_TENANT=` left in a profile must
        # not name the default tenant, which a bound credential is refused
        # (and which would narrow an unbound one's reads to it).
        client = Client.from_env({"TRAZA_URL": AUTH.url, "TRAZA_TOKEN": ACME_RO, "TRAZA_TENANT": ""})
        self.assertIsNone(client.tenant)
        self.assertEqual(client.check("r1").tenant, "acme")
        # An explicit "" still names the default tenant, which this
        # credential may not ask about.
        with self.assertRaises(AuthError):
            Client(AUTH.url, token=ACME_RO, tenant="").check("r1")
        self.assertIsNone(Client.from_env({"TRAZA_TENANT": ""}).tenant)


# Runs this module's fixture in a child interpreter with one step made to
# fail, and reports which of the servers it started outlived the failure.
# argv: the tests directory, and which step fails.
_FIXTURE_PROBE = """
import json, os, sys, unittest
sys.path.insert(0, sys.argv[1])
import test_client

failing = sys.argv[2]
started = []

class Recorded(test_client.Server):
    def __init__(self, *args, **kwargs):
        if failing == "second server" and kwargs.get("env"):
            raise RuntimeError("the second server did not start")
        super().__init__(*args, **kwargs)
        started.append(self)

test_client.Server = Recorded
if failing == "seed":
    # An ingest regression, as the fixture meets it: the seed batch refused.
    test_client.weather_run = lambda *args, **kwargs: {"not": "a batch"}
result = unittest.TestResult()
try:
    unittest.defaultTestLoader.loadTestsFromTestCase(test_client.Configuration).run(result)
    print(json.dumps({
        "errors": [str(test) for test, _ in result.errors],
        "started": len(started),
        "left": [s.port for s in started if s.process.poll() is None or os.path.isdir(s.data_dir)],
    }))
finally:
    for server in started:
        server.stop()
"""


class ModuleFixture(unittest.TestCase):
    def test_a_failed_set_up_leaves_no_server_behind(self):
        # unittest skips tearDownModule when setUpModule raises, so stopping
        # the servers there would leave both processes and their stores behind
        # whenever a seed request is refused. The fixture is run in a child
        # interpreter because module cleanups live in one process-wide list:
        # a nested run here would also run, and stop, this module's own.
        for failing, servers in (("seed", 2), ("second server", 1)):
            with self.subTest(failing):
                child = subprocess.run(
                    [sys.executable, "-c", _FIXTURE_PROBE, str(Path(__file__).resolve().parent), failing],
                    capture_output=True,
                    text=True,
                    timeout=120,
                )
                self.assertEqual(child.returncode, 0, child.stderr)
                report = json.loads(child.stdout.splitlines()[-1])
                self.assertEqual(report["errors"], ["setUpModule (test_client)"], child.stdout)
                self.assertEqual(report["started"], servers)
                self.assertEqual(report["left"], [], "a server outlived the failed setUpModule")


if __name__ == "__main__":
    unittest.main()
