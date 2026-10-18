"""Tape tests: prove an agent still takes a recorded path, with zero model calls.

A recorded run is a regression test waiting to happen. Replay it strictly —
every model call the agent makes is answered from the recording, and must
ask exactly what the recording asked — and the agent either walks the same
path or says where it left it::

    from traza import Client
    from traza.testing import replay_test

    def test_weather_agent_still_asks_for_the_tool():
        replay_test(Client.from_env(), "weather-7", lambda tape: run_agent("Paris"))

A tape test fails when the agent's request at some step differs from the
recorded one (the failure names the message, field and character where it
first differs), when it stops before the recording's last model call, or
when it asks for more than the recording holds. Nothing is sent to a model
provider: the replay answers in its place.

:class:`TapeTest` is the context manager underneath; the pytest plugin
(:mod:`traza.pytest_plugin`, ``@pytest.mark.traza_tape("run-id")``) and the
``traza test`` command (:mod:`traza.cli`), which replays every golden run and
writes a JUnit XML report, are built on it. Standard library only.
"""

from __future__ import annotations

import os
import re
import subprocess
import time
import xml.etree.ElementTree as ET
from dataclasses import dataclass
from typing import Any, Callable, Dict, List, Mapping, Optional, Sequence

from .client import Client
from .errors import TrazaError
from .models import Replay, ReplayStatus, Tape, ToolStep

__all__ = [
    "TapeMismatch",
    "TapeTest",
    "GoldenResult",
    "describe",
    "replay_key",
    "replay_test",
    "first_user_input",
    "run_golden",
    "junit_xml",
]

#: Bytes of an agent command's stdout and stderr a golden result keeps: the
#: tail, where a failure's last words are.
OUTPUT_TAIL_BYTES = 64 * 1024

#: The environment variable holding the key an agent under test is handed as
#: its provider API key, in place of the runner's own ``TRAZA_TOKEN``.
REPLAY_KEY_ENV = "TRAZA_REPLAY_KEY"


def replay_key(client: Client, environ: Optional[Mapping[str, str]] = None) -> Optional[str]:
    """The key an agent under test sends as ``OPENAI_API_KEY`` /
    ``ANTHROPIC_API_KEY`` — ``None`` meaning the client's own token.

    ``TRAZA_REPLAY_KEY`` when set: a dedicated ``ro`` credential bound to the
    run's tenant is the right thing to hand an agent, since any library that
    reads ``OPENAI_API_KEY`` but ignores ``OPENAI_BASE_URL`` sends it to the
    real provider. Otherwise the client's token is used — unless it is an
    unbound (operator) credential, which can read every tenant: that is
    refused with ``ValueError``, naming the remedy. To hand it over anyway,
    say so: ``TRAZA_REPLAY_KEY=$TRAZA_TOKEN``.
    """
    environ = os.environ if environ is None else environ
    dedicated = environ.get(REPLAY_KEY_ENV)
    if dedicated:
        return dedicated
    if not client.token:
        return None
    if client.whoami().operator:
        raise ValueError(
            "TRAZA_TOKEN is an unbound (operator) credential, and an agent under test would get it as "
            "OPENAI_API_KEY / ANTHROPIC_API_KEY, where a library that ignores OPENAI_BASE_URL sends it to "
            f"the provider. Set {REPLAY_KEY_ENV} to a ro credential bound to the runs' tenant (or to "
            "$TRAZA_TOKEN to accept the exposure)"
        )
    return None


class TapeMismatch(AssertionError):
    """The agent did not take the recorded path. ``status`` is the replay's
    status document, the evidence."""

    def __init__(self, message: str, status: Optional[ReplayStatus] = None) -> None:
        super().__init__(message)
        self.status = status


def _quote(text: Any) -> str:
    return repr(text) if isinstance(text, str) else "nothing"


def _difference_lines(difference: Mapping[str, Any]) -> List[str]:
    index = difference.get("message")
    role = difference.get("role") or "?"
    field = difference.get("field")
    recorded, got = difference.get("recorded"), difference.get("got")
    lines: List[str] = []
    if field == "missing":
        lines.append(f"  message {index} ({role}) is missing from the request; the recording has {_quote(recorded)}")
    elif field == "extra":
        lines.append(f"  the request has an extra message {index} ({role}): {_quote(got)}")
    else:
        lines.append(f"  message {index} ({role}): {field} differs at character {difference.get('at_char')}")
        lines.append(f"    recorded: {_quote(recorded)}")
        lines.append(f"    got:      {_quote(got)}")
    lines.append(
        f"  (the recording's request has {difference.get('recorded_messages')} messages, "
        f"the agent's {difference.get('got_messages')})"
    )
    return lines


def describe(status: ReplayStatus, run: Optional[str] = None) -> Optional[str]:
    """Why a replayed run failed its tape — or ``None`` when the agent made
    exactly the recorded model calls, each asking what the recording asked."""
    run = run or status.run or "?"
    diverged = status.diverged
    if diverged is not None:
        lines = [f"the agent diverged from recorded run {run} at step {diverged.step}:"]
        if diverged.difference:
            lines.extend(_difference_lines(diverged.difference))
        else:
            lines.append(
                f"  its request digests to {(diverged.got_digest or '?')[:12]}, the recording's to "
                f"{(diverged.expected_digest or '?')[:12]} (the recorded request could not be re-read "
                "to say where they differ)"
            )
        return "\n".join(lines)
    served = status.served or 0
    remaining = status.remaining or 0
    if remaining > 0:
        return (
            f"the agent stopped after {served} of the {served + remaining} model calls recorded in "
            f"run {run}: the recording expected {remaining} more"
        )
    if status.exhausted_calls:
        return (
            f"the agent made {status.exhausted_calls} model call(s) past the end of recorded run {run}, "
            f"which holds {served}"
        )
    return None


class TapeTest:
    """One strict replay of ``run``, set up for an agent to run against.

    Entering creates the replay and (unless ``set_env=False``) points
    ``OPENAI_BASE_URL``, ``ANTHROPIC_BASE_URL`` and their API keys at it, and
    sets ``TRAZA_RUN`` and ``TRAZA_REPLAY_ID``; leaving restores them and
    reads the replay's status. :meth:`failure` then says whether the agent
    took the recorded path, and :meth:`check` raises :class:`TapeMismatch`
    when it did not. ``api_key`` is the key the agent's SDK sends (default:
    the client's token); see :meth:`traza.models.Replay.env`.
    """

    def __init__(
        self,
        client: Client,
        run: str,
        mode: str = "strict",
        *,
        set_env: bool = True,
        api_key: Optional[str] = None,
    ) -> None:
        self.client = client
        self.run = run
        self.mode = mode
        self.set_env = set_env
        self.api_key = api_key
        self.replay: Optional[Replay] = None
        #: The replay's status as the block ended.
        self.status: Optional[ReplayStatus] = None
        self._saved: Dict[str, Optional[str]] = {}

    def env(self) -> Dict[str, str]:
        """The variables :meth:`__enter__` sets — for a subprocess agent."""
        replay = self._replay()
        env = replay.env(self.api_key)
        env["TRAZA_RUN"] = self.run
        env["TRAZA_REPLAY_ID"] = replay.id or ""
        return env

    def __enter__(self) -> "TapeTest":
        self.replay = self.client.replay(self.run, mode=self.mode)
        if self.set_env:
            for key, value in self.env().items():
                self._saved[key] = os.environ.get(key)
                os.environ[key] = value
        return self

    def __exit__(self, *exc_info: Any) -> None:
        for key, value in self._saved.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value
        self._saved = {}
        try:
            self.status = self._replay().status()
        except TrazaError:
            self.status = None

    def tool_result(self, name: Optional[str] = None) -> Optional[ToolStep]:
        """The next recorded tool result (of ``name``), for a harness that
        replays tools too; ``None`` when none is left."""
        return self._replay().next_tool(name)

    def failure(self) -> Optional[str]:
        """Why the agent failed the tape, or ``None``."""
        status = self.status or self._replay().status()
        return describe(status, self.run)

    def check(self) -> ReplayStatus:
        """Raises :class:`TapeMismatch` unless the agent took the recorded
        path; returns the status."""
        status = self.status or self._replay().status()
        failure = describe(status, self.run)
        if failure is not None:
            raise TapeMismatch(failure, status)
        return status

    def _replay(self) -> Replay:
        if self.replay is None:
            raise RuntimeError("the tape test has not started; use it as a context manager")
        return self.replay


def replay_test(
    client: Client,
    run: str,
    agent: Callable[[TapeTest], Any],
    mode: str = "strict",
    **options: Any,
) -> ReplayStatus:
    """Runs ``agent`` against a strict replay of ``run`` and returns the
    replay's status — or raises :class:`TapeMismatch` saying where the agent
    left the recorded path. An exception the agent raised because it was
    refused (a divergence answers ``409``) is chained beneath the mismatch,
    which says what the refusal meant; any other exception propagates."""
    tape = TapeTest(client, run, mode, **options)
    with tape:
        try:
            agent(tape)
        except Exception as error:
            failure = tape.failure()
            if failure is not None:
                raise TapeMismatch(failure, tape.status) from error
            raise
    return tape.check()


def first_user_input(tape: Tape) -> Optional[str]:
    """The text of the last user message of the run's first model call —
    for most agents, the task they were given. Best effort: ``None`` when
    the request is not a message list, or holds no user text."""
    for step in tape.steps:
        if step.kind != "llm":
            continue
        request = step.request
        if not isinstance(request, list):
            return None
        for message in reversed(request):
            if isinstance(message, dict) and message.get("role") == "user":
                return _text_of(message)
        return None
    return None


def _text_of(message: Mapping[str, Any]) -> Optional[str]:
    content = message.get("content")
    if isinstance(content, str):
        return content
    parts = content if isinstance(content, list) else message.get("parts")
    if isinstance(parts, list):
        texts = [
            part.get("text") if isinstance(part.get("text"), str) else part.get("content")
            for part in parts
            if isinstance(part, dict) and part.get("type") in ("text", "input_text", None)
        ]
        joined = "".join(text for text in texts if isinstance(text, str))
        return joined or None
    return None


# ---------------------------------------------------------------- golden runs


@dataclass
class GoldenResult:
    """One golden run replayed against an agent command."""

    run: str
    tenant: Optional[str]
    #: ``True`` when the agent took the recorded path and exited 0.
    passed: bool
    #: ``None`` when passed; else ``divergence``, ``incomplete``, ``overrun``,
    #: ``exit``, ``timeout`` — failures — or ``error``, when the run could not
    #: be tested at all (a replay that could not be created).
    kind: Optional[str]
    #: The failure, in words.
    message: Optional[str]
    seconds: float
    replay_id: Optional[str] = None
    exit_code: Optional[int] = None
    stdout: str = ""
    stderr: str = ""


def _tail(data: Optional[bytes]) -> str:
    if not data:
        return ""
    clipped = data[-OUTPUT_TAIL_BYTES:]
    text = clipped.decode("utf-8", "replace")
    return ("…" + text) if len(data) > OUTPUT_TAIL_BYTES else text


def _kind(status: ReplayStatus) -> str:
    if status.diverged is not None:
        return "divergence"
    if (status.remaining or 0) > 0:
        return "incomplete"
    return "overrun"


def run_golden(
    client: Client,
    run: str,
    command: Sequence[str],
    *,
    tenant: Optional[str] = None,
    mode: str = "strict",
    timeout: float = 300.0,
    api_key: Optional[str] = None,
) -> GoldenResult:
    """Replays ``run`` under ``command``, an agent that makes the run's model
    calls through the replay, and judges it.

    The command runs with the replay's environment (``OPENAI_BASE_URL`` and
    friends, ``TRAZA_RUN``, ``TRAZA_REPLAY_ID``) plus ``TRAZA_RUN_INPUT``, the
    run's first user message when one can be read; ``{run}``, ``{input}`` and
    ``{replay}`` in its arguments are replaced by the same values. It passes
    when it exits 0 having made exactly the recorded model calls.
    """
    started = time.monotonic()
    scoped = Client(client.url, token=client.token, tenant=tenant if tenant is not None else client.tenant,
                    timeout=client.timeout)
    try:
        replay = scoped.replay(run, mode=mode)
    except TrazaError as error:
        return GoldenResult(run, tenant, False, "error", f"the replay could not be created: {error}",
                            time.monotonic() - started)
    try:
        task = first_user_input(scoped.tape(run, resolve=True, max_steps=32)) or ""
    except TrazaError:
        task = ""
    env = dict(os.environ)
    env.update(replay.env(api_key))
    env.update({"TRAZA_RUN": run, "TRAZA_REPLAY_ID": replay.id or "", "TRAZA_RUN_INPUT": task})
    argv = [
        part.replace("{run}", run).replace("{input}", task).replace("{replay}", replay.id or "")
        for part in command
    ]
    exit_code: Optional[int] = None
    out: Optional[bytes] = None
    err: Optional[bytes] = None
    timed_out = False
    try:
        finished = subprocess.run(argv, env=env, stdin=subprocess.DEVNULL, capture_output=True, timeout=timeout)
        exit_code, out, err = finished.returncode, finished.stdout, finished.stderr
    except subprocess.TimeoutExpired as expired:
        timed_out = True
        out = expired.stdout if isinstance(expired.stdout, bytes) else None
        err = expired.stderr if isinstance(expired.stderr, bytes) else None
    except OSError as error:
        return GoldenResult(run, tenant, False, "error", f"the agent command could not be started: {error}",
                            time.monotonic() - started, replay.id)
    seconds = time.monotonic() - started
    result = GoldenResult(run, tenant, False, None, None, seconds, replay.id, exit_code, _tail(out), _tail(err))
    try:
        status = replay.status()
    except TrazaError as error:
        result.kind, result.message = "error", f"the replay's status could not be read: {error}"
        return result
    failure = describe(status, run)
    if failure is not None:
        result.kind, result.message = _kind(status), failure
    elif timed_out:
        result.kind, result.message = "timeout", f"the agent command did not finish within {timeout:g} s"
    elif exit_code != 0:
        result.kind = "exit"
        result.message = f"the agent took the recorded path but exited {exit_code}"
    else:
        result.passed = True
    return result


_INVALID_XML = re.compile("[\x00-\x08\x0b\x0c\x0e-\x1f￾￿]")


def _xml_text(text: Optional[str]) -> str:
    return _INVALID_XML.sub("\N{REPLACEMENT CHARACTER}", text or "")


def junit_xml(results: Sequence[GoldenResult], suite: str = "traza.golden") -> str:
    """The results as a JUnit XML report: one ``testcase`` per golden run,
    named by run id, classed ``traza.golden`` (``.<tenant>`` for a named
    tenant), with a ``failure`` (or, for a run that could not be tested, an
    ``error``) of the result's kind and message, and the agent's output tail
    as ``system-out`` / ``system-err``."""
    failures = sum(1 for result in results if not result.passed and result.kind != "error")
    errors = sum(1 for result in results if result.kind == "error")
    total = f"{sum(result.seconds for result in results):.3f}"
    root = ET.Element("testsuites", name="traza", tests=str(len(results)), failures=str(failures),
                      errors=str(errors), time=total)
    stamp = time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime())
    node = ET.SubElement(root, "testsuite", name=suite, tests=str(len(results)), failures=str(failures),
                         errors=str(errors), skipped="0", time=total, timestamp=stamp)
    for result in results:
        classname = suite + (f".{result.tenant}" if result.tenant else "")
        case = ET.SubElement(node, "testcase", classname=_xml_text(classname), name=_xml_text(result.run),
                             time=f"{result.seconds:.3f}")
        if not result.passed:
            tag = "error" if result.kind == "error" else "failure"
            message = result.message or ""
            element = ET.SubElement(case, tag, type=result.kind or "failure",
                                    message=_xml_text(message.splitlines()[0] if message else ""))
            element.text = _xml_text(message)
        out = f"replay {result.replay_id}\n" if result.replay_id else ""
        if result.stdout or out:
            ET.SubElement(case, "system-out").text = _xml_text(out + result.stdout)
        if result.stderr:
            ET.SubElement(case, "system-err").text = _xml_text(result.stderr)
    return '<?xml version="1.0" encoding="UTF-8"?>\n' + ET.tostring(root, encoding="unicode") + "\n"
