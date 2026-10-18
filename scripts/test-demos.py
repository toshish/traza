#!/usr/bin/env python3
"""Prove the demos keep their tokens to themselves, and that CI runs them as
a user would.

- The demo agents send bearer tokens with urllib, which follows a redirect
  with every header the request carried, Authorization included: to another
  host, and from https to plain http. A proxy answering 302 in front of
  Traza or a model provider would hand the token to wherever it pointed.
- ci.sh runs the demos as merge gates. An exported TRAZA_BIN naming an older
  install would point several of them at that binary instead of the one just
  built, so they could pass while the branch was broken; an exported
  TRAZA_SEED_BIN would do the same to the corpus foresight writes. And a
  script that splits its own path on spaces works from every checkout
  without one.
"""

import http.server
import importlib.util
import json
import os
from pathlib import Path
import re
import socketserver
import subprocess
import sys
import tempfile
import threading
import time
import unittest

ROOT = Path(__file__).resolve().parent.parent
TOKEN = "trz_demo_secret_0123456789"
STATUSES = (301, 302, 303, 307, 308)
# A proxy configured on this machine must not stand in for the loopback
# servers below, in this process or in the agents it starts.
os.environ["no_proxy"] = "*"


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class Loopback(http.server.ThreadingHTTPServer):
    def server_bind(self):
        # HTTPServer.server_bind asks for the address's fully qualified name,
        # a reverse lookup that costs seconds per server on a macOS CI runner;
        # nothing here uses it.
        socketserver.TCPServer.server_bind(self)
        self.server_name, self.server_port = self.server_address[:2]


class Server:
    """A loopback HTTP server that answers every request the same way and
    keeps what each one carried."""

    def __init__(self, status, headers=None, body=b"{}"):
        self.requests = []
        requests = self.requests

        class Handler(http.server.BaseHTTPRequestHandler):
            def answer(self):
                length = int(self.headers.get("Content-Length") or 0)
                requests.append({"method": self.command, "path": self.path,
                                 "headers": dict(self.headers), "body": self.rfile.read(length)})
                self.send_response(status)
                for name, value in (headers or {}).items():
                    self.send_header(name, value)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            do_GET = do_POST = do_HEAD = answer

            def log_message(self, *args):
                pass

        self.httpd = Loopback(("127.0.0.1", 0), Handler)
        self.port = self.httpd.server_port
        self.url = "http://127.0.0.1:%d" % self.port

    def __enter__(self):
        # shutdown() waits for the loop to poll; the default half second per
        # server would be most of this suite's run time.
        threading.Thread(target=self.httpd.serve_forever, args=(0.02,), daemon=True).start()
        return self

    def __exit__(self, *exc):
        self.httpd.shutdown()
        self.httpd.server_close()


def run_agent(path, *args, env=None):
    # Only the configuration each test names: nothing the agents read may
    # leak in from the shell this runs in.
    inherited = {k: v for k, v in os.environ.items()
                 if not k.startswith(("TRAZA_", "OPENAI_", "WEATHER_"))}
    return subprocess.run([sys.executable, "-I", str(path), *map(str, args)],
                          env=dict(inherited, **(env or {})), capture_output=True,
                          text=True, timeout=30)


class MissionControlHolds(unittest.TestCase):
    """A Mission Control agent the pre-call check refuses waits for release
    under ONE deadline, and counts a release only when the check lets the
    step it was refused go ahead.

    The check judges the path with the next step added; the verdict read
    the agent polls while it holds judges the path so far. A stop on the
    step alone — a predictive limit judging the named tool, a block nudge —
    leaves the verdict read saying continue, which is no release."""

    HOLD = 90

    def setUp(self):
        self.agents = load("mission_agents", ROOT / "examples/mission-control/agents.py")
        clock = self.clock = [1_000_000.0]
        limit = clock[0] + 10 * self.HOLD

        class Overran(Exception):
            pass

        def sleep(seconds):
            clock[0] += max(seconds, 0.001)
            if clock[0] > limit:
                raise Overran("still holding after %d s of a %d s hold" % (clock[0] - 1_000_000, self.HOLD))

        self.agents.time = type("FakeTime", (), {"time": staticmethod(lambda: clock[0]), "sleep": staticmethod(sleep)})
        self.agents.say = lambda *parts: None
        self.agents.warn = lambda *parts: None

    def api(self, check, verdict):
        """A server whose check answers `check(n)` to its nth call and whose
        verdict read answers `verdict(seconds since the start)`."""
        clock, start = self.clock, self.clock[0]

        class Api:
            checks = 0
            reads = 0
            spans = []

            def post(self, path, body):
                if path == "/v1/fuses/check":
                    Api.checks += 1
                    stop = check(Api.checks)
                    return 200, {"verdict": "stop" if stop else "continue",
                                 "reasons": ["foresight: the named step's odds are 4%"] if stop else [], "nudges": []}
                Api.spans.extend(body.get("spans", []))
                return 200, {}

            def get(self, path):
                Api.reads += 1
                return 200, {"verdict": verdict(clock[0] - start)}

        return Api()

    def test_a_stop_on_the_next_step_alone_abandons_at_the_deadline(self):
        for kind in ("Run", "TeamRun"):
            with self.subTest(kind=kind):
                start = self.clock[0]
                api = self.api(check=lambda n: True, verdict=lambda elapsed: "continue")
                run = getattr(self.agents, kind)(api, "run-" + kind, "refund" if kind == "Run" else "handoff", 1.0, self.HOLD)
                run.work()
                self.assertEqual(run.outcome, "abandoned")
                self.assertEqual(run.position, 0, "it took a step the check refused")
                self.assertEqual(run.released, 0, "a verdict read of the path so far is not a release")
                self.assertEqual(len(run.stops), 1, "one refused step is one hold")
                self.assertLessEqual(self.clock[0] - start, self.HOLD + 5)
                self.assertFalse(run.holding)

    def test_a_release_is_counted_when_the_check_lets_the_step_go(self):
        # Stopped for the path so far (the verdict read says stop too) until
        # someone resets the run 20 s in; the check then lets the step go.
        released_at = 20
        api = self.api(check=lambda n: n == 1 or self.clock[0] - start < released_at,
                       verdict=lambda elapsed: "stop" if elapsed < released_at else "continue")
        start = self.clock[0]
        run = self.agents.Run(api, "run-released", "refund", 1.0, self.HOLD)
        run.work()
        self.assertEqual(run.released, 1)
        self.assertEqual(len(run.stops), 1)
        self.assertEqual(run.position, len(run.steps))
        self.assertIn(run.outcome, ("success", "failure"))
        self.assertFalse(run.holding)


class RedirectsAreRefused(unittest.TestCase):
    """Every authenticated request a demo agent makes refuses a 3xx: the
    token reaches only the URL the agent was configured with."""

    def assert_refused(self, result, location, elsewhere):
        followed = list(elsewhere.requests)
        elsewhere.requests.clear()
        self.assertEqual(followed, [], "a redirect was followed, token and all")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn(location, result.stderr)
        self.assertNotIn("Traceback", result.stderr)

    def test_the_guard_harness_refuses_on_every_route_and_method(self):
        guard = load("guard_agent", ROOT / "examples/guard/agent.py")
        with Server(200) as elsewhere:
            location = "http://localhost:%d/elsewhere" % elsewhere.port
            for status in STATUSES:
                with Server(status, {"Location": location}, b"moved") as front:
                    for method, body in (("GET", None), ("POST", {"run": "r1"})):
                        with self.subTest(status=status, method=method):
                            try:
                                guard.request(front.url, TOKEN, method, "/v1/fuses/check", body)
                                refusal = "answered, not refused"
                            except Exception as error:  # the refusal's type is the agent's own
                                refusal = str(error)
                            followed = list(elsewhere.requests)
                            elsewhere.requests.clear()
                            self.assertEqual(followed, [], "a redirect was followed, token and all")
                            self.assertIn(location, refusal)
                    self.assertEqual(len(front.requests), 2)
                    self.assertEqual(front.requests[0]["headers"].get("Authorization"),
                                     "Bearer " + TOKEN)

    def test_the_guard_agent_stops_with_a_message(self):
        with Server(200) as elsewhere, tempfile.TemporaryDirectory() as tmp:
            location = "http://localhost:%d/elsewhere" % elsewhere.port
            pricing = Path(tmp) / "pricing.json"
            pricing.write_text(json.dumps(
                {"models": {"sim-large": {"input_per_mtok": 3.0, "output_per_mtok": 15.0}}}))
            with Server(302, {"Location": location}, b"moved") as front:
                result = run_agent(ROOT / "examples/guard/agent.py", "agent", front.url, TOKEN,
                                   "r1", "summarise", Path(tmp) / "state.json", pricing, "up", 1,
                                   Path(tmp) / "out-")
            self.assert_refused(result, location, elsewhere)

    def test_the_replay_agent_refuses_for_the_model_and_for_traza(self):
        agent = ROOT / "examples/replay/agent.py"
        answer = json.dumps({
            "model": "sim-4o", "usage": {"prompt_tokens": 9, "completion_tokens": 7},
            "choices": [{"finish_reason": "stop",
                         "message": {"role": "assistant", "content": "Raining, 12 C."}}],
        }).encode()
        with Server(200) as elsewhere, tempfile.TemporaryDirectory() as tmp:
            location = "http://localhost:%d/elsewhere" % elsewhere.port
            result_file = Path(tmp) / "result.json"
            for status in STATUSES:
                with self.subTest(status=status, call="model"):
                    with Server(status, {"Location": location}, b"moved") as front:
                        result = run_agent(agent, result_file, env={
                            "OPENAI_BASE_URL": front.url + "/v1", "OPENAI_API_KEY": TOKEN})
                    self.assertEqual(front.requests[0]["headers"].get("Authorization"),
                                     "Bearer " + TOKEN)
                    self.assert_refused(result, location, elsewhere)
                with self.subTest(status=status, call="telemetry"):
                    with Server(200, {"Content-Type": "application/json"}, answer) as model, \
                            Server(status, {"Location": location}, b"moved") as front:
                        result = run_agent(agent, result_file, env={
                            "OPENAI_BASE_URL": model.url + "/v1", "OPENAI_API_KEY": "model-key",
                            "TRAZA_URL": front.url, "TRAZA_TOKEN": TOKEN, "TRAZA_RUN": "r1"})
                    self.assertEqual(front.requests[0]["headers"].get("Authorization"),
                                     "Bearer " + TOKEN)
                    self.assert_refused(result, location, elsewhere)

    def test_the_foresight_harness_and_the_watch_poller_refuse(self):
        foresight = load("foresight_agent", ROOT / "examples/foresight/agent.py")
        coding = load("coding_agents_render", ROOT / "examples/coding-agents/render.py")
        calls = {
            "foresight check": lambda url: foresight.request(url, TOKEN, "POST", "/v1/fuses/check",
                                                             {"run": "r1"}),
            "foresight read": lambda url: foresight.request(url, TOKEN, "GET", "/v1/fate/runs/r1"),
            "watch poll": lambda url: coding.request(url, TOKEN, "/v1/tapes/r1"),
        }
        with Server(200) as elsewhere:
            location = "http://localhost:%d/elsewhere" % elsewhere.port
            for status in STATUSES:
                with Server(status, {"Location": location}, b"moved") as front:
                    for name, call in calls.items():
                        with self.subTest(status=status, call=name):
                            try:
                                call(front.url)
                                refusal = "answered, not refused"
                            except Exception as error:  # the refusal's type is the module's own
                                refusal = str(error)
                            followed = list(elsewhere.requests)
                            elsewhere.requests.clear()
                            self.assertEqual(followed, [], "a redirect was followed, token and all")
                            self.assertIn(location, refusal)
                    self.assertEqual(len(front.requests), len(calls))
                    for sent in front.requests:
                        self.assertEqual(sent["headers"].get("Authorization"), "Bearer " + TOKEN)

    def test_the_foresight_agent_and_the_watch_poller_stop_with_a_message(self):
        with Server(200) as elsewhere, tempfile.TemporaryDirectory() as tmp:
            location = "http://localhost:%d/elsewhere" % elsewhere.port
            with Server(302, {"Location": location}, b"moved") as front:
                result = run_agent(ROOT / "examples/foresight/agent.py", "run", front.url, TOKEN,
                                   "r1", "doomed", Path(tmp) / "state.json", Path(tmp) / "out-")
                self.assert_refused(result, location, elsewhere)
                result = run_agent(ROOT / "examples/coding-agents/render.py", "arrived", front.url,
                                   TOKEN, "r1", 1, "%.6f" % time.time(), Path(tmp) / "tape.json")
                self.assert_refused(result, location, elsewhere)

    def test_no_demo_sends_a_token_through_the_default_opener(self):
        # The default opener follows redirects. A demo that builds requests
        # with urllib and sets Authorization must send them through its own.
        for path in sorted(ROOT.glob("examples/*/*.py")):
            source = path.read_text()
            if "urllib.request" in source and "Authorization" in source:
                with self.subTest(path=str(path.relative_to(ROOT))):
                    self.assertNotIn("urlopen(", source)


class CiRunsTheDemos(unittest.TestCase):
    def test_every_demo_runs_the_built_binary_from_a_path_with_a_space(self):
        ci = (ROOT / "ci.sh").read_text()
        block = re.search(r"^# -+ examples\n(.*?)^# -+ client libraries$", ci,
                          re.MULTILINE | re.DOTALL)
        self.assertIsNotNone(block, "ci.sh has no examples section")
        demos = sorted(path.parent.name for path in ROOT.glob("examples/*/run.sh"))
        def reading(variable):
            return {d for d in demos if variable in (ROOT / "examples" / d / "run.sh").read_text()}

        honours, seeds = reading("TRAZA_BIN"), reading("TRAZA_SEED_BIN")
        self.assertTrue(honours, "no demo reads TRAZA_BIN; this test would check nothing")
        self.assertTrue(seeds, "no demo reads TRAZA_SEED_BIN; this test would check nothing")
        with tempfile.TemporaryDirectory() as tmp:
            checkout = Path(tmp) / "checkout"
            log = Path(tmp) / "ran.log"
            # Each stub records how it was invoked and the files TRAZA_BIN and
            # TRAZA_SEED_BIN named, resolved while it runs: ci.sh removes its
            # symlink on exit.
            record = ("import os, sys; name, invoked, *binaries = sys.argv[1:]; "
                      "open(os.environ['TRAZA_TEST_LOG'], 'a').write('\\t'.join("
                      "[name, invoked] + [b and os.path.realpath(b) for b in binaries]) + '\\n')")
            for demo in demos:
                stub = checkout / "examples" / demo / "run.sh"
                stub.parent.mkdir(parents=True)
                stub.write_text('#!/bin/sh\nexec "%s" -I -c "%s" %s "$0" "${TRAZA_BIN-}" '
                                '"${TRAZA_SEED_BIN-}"\n' % (sys.executable, record, demo))
                stub.chmod(0o755)
            built = checkout / "target/release/traza-server"
            built.parent.mkdir(parents=True)
            built.write_text("")
            seed = checkout / "target/release/seed"
            seed.write_text("")
            scratch = Path(tmp) / "tmp"
            scratch.mkdir()
            # What a developer's shell might export: an older install.
            stale = Path(tmp) / "old-release"
            subprocess.run(["bash", "-c", "set -euo pipefail\n" + block.group(1)], cwd=checkout,
                           env=dict(os.environ, TRAZA_BIN=str(stale / "traza-server"),
                                    TRAZA_SEED_BIN=str(stale / "seed"), TRAZA_TEST_LOG=str(log),
                                    TMPDIR=str(scratch)),
                           check=True, capture_output=True, timeout=60)
            rows = [line.split("\t") for line in log.read_text().splitlines()]
            self.assertEqual(sorted(row[0] for row in rows), demos,
                             "every demo with a run.sh runs, once")
            for name, invoked, traza_bin, seed_bin in rows:
                with self.subTest(demo=name):
                    self.assertIn(" ", invoked, "a checkout path with a space is never exercised")
                    if name in honours:
                        self.assertEqual(traza_bin, os.path.realpath(built),
                                         "TRAZA_BIN must name the binary just built")
                    if name in seeds:
                        self.assertEqual(seed_bin, os.path.realpath(seed),
                                         "TRAZA_SEED_BIN must name the seed just built")
            self.assertEqual(os.listdir(scratch), [], "ci.sh left its spaced path behind")


if __name__ == "__main__":
    unittest.main()
