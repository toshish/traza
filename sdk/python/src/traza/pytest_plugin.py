"""pytest-traza: recorded runs as tests.

Installed with the ``traza`` package and registered under pytest's ``pytest11``
entry point, so it is active whenever the package is installed (or load it
explicitly with ``-p traza.pytest_plugin``). Mark a test with the run it must
reproduce::

    import pytest

    @pytest.mark.traza_tape("dr-0917")
    def test_research_agent_follows_its_recording(traza_tape):
        run_research(TASK, recorded=traza_tape.tool_result)   # every model call answered from the run

For the test's duration ``OPENAI_BASE_URL``, ``ANTHROPIC_BASE_URL`` and their
API keys point at a strict replay of the run, so an agent that builds its
OpenAI or Anthropic client inside the test talks to the recording, with zero
model calls. The test fails — with the message, field and character where the
agent's request first differed from the recording — when the agent leaves the
recorded path, stops before the recording ends, or asks for more than it
holds. ``mode="loose"`` serves the recording in order regardless.

The ``traza_tape`` fixture hands the test the :class:`traza.testing.TapeTest`
(its replay, recorded tool results via ``tool_result()``, the environment for
a subprocess agent). The server is ``--traza-url`` (default ``TRAZA_URL``,
else ``http://127.0.0.1:8080``); the credential is ``TRAZA_TOKEN`` and the
tenant ``--traza-tenant`` or ``TRAZA_TENANT``. A token is never a command-line
option: it would land in process listings and CI logs. The agent's provider
API key is ``TRAZA_REPLAY_KEY`` when set (a ``ro`` credential of the run's
tenant), else ``TRAZA_TOKEN`` — refused when that is an unbound (operator)
credential (:func:`traza.testing.replay_key`).

A marked test that skips itself, or is ``xfail`` and fails as expected, is
reported as it was: the tape is judged only for a test that ran to an end.
"""

from __future__ import annotations

import os
from typing import Any, Iterator, Optional

import pytest

from .client import Client
from .testing import TapeTest, replay_key

__all__ = ["traza_tape"]

_KEY = "_traza_tape"
_REPLAY_KEY = "_traza_replay_key"


def pytest_addoption(parser: Any) -> None:
    group = parser.getgroup("traza", "replay recorded agent runs as tests")
    group.addoption("--traza-url", default=None, help="the Traza server (default: $TRAZA_URL)")
    group.addoption("--traza-tenant", default=None, help="the tenant to replay under (default: $TRAZA_TENANT)")


def pytest_configure(config: Any) -> None:
    config.addinivalue_line(
        "markers",
        'traza_tape(run, mode="strict"): run the test against a strict replay of a recorded '
        "Traza run; fail it, naming the difference, if the agent leaves the recorded path",
    )


def _client(config: Any) -> Client:
    url = config.getoption("--traza-url") or os.environ.get("TRAZA_URL") or "http://127.0.0.1:8080"
    tenant = config.getoption("--traza-tenant") or os.environ.get("TRAZA_TENANT") or None
    return Client(url, token=os.environ.get("TRAZA_TOKEN") or None, tenant=tenant)


@pytest.fixture(autouse=True)
def _traza_tape_marker(request: Any) -> Iterator[None]:
    marker = request.node.get_closest_marker("traza_tape")
    if marker is None:
        yield
        return
    if not marker.args or not isinstance(marker.args[0], str) or not marker.args[0]:
        raise pytest.UsageError('traza_tape needs the run to replay: @pytest.mark.traza_tape("<run id>")')
    mode = marker.kwargs.get("mode", "strict")
    client = _client(request.config)
    # Asked once per session: the credential does not change between tests.
    if not hasattr(request.config, _REPLAY_KEY):
        try:
            setattr(request.config, _REPLAY_KEY, (replay_key(client), None))
        except ValueError as refused:
            setattr(request.config, _REPLAY_KEY, (None, str(refused)))
    key, refusal = getattr(request.config, _REPLAY_KEY)
    if refusal is not None:
        raise pytest.UsageError(f"traza_tape: {refusal}")
    tape = TapeTest(client, marker.args[0], mode=mode, api_key=key)
    setattr(request.node, _KEY, tape)
    with tape:
        yield


@pytest.fixture
def traza_tape(request: Any, _traza_tape_marker: None) -> TapeTest:
    """The test's :class:`~traza.testing.TapeTest`; the test must carry the
    ``traza_tape`` marker."""
    tape: Optional[TapeTest] = getattr(request.node, _KEY, None)
    if tape is None:
        raise pytest.UsageError('the traza_tape fixture needs @pytest.mark.traza_tape("<run id>") on the test')
    return tape


@pytest.hookimpl(hookwrapper=True)
def pytest_runtest_makereport(item: Any, call: Any) -> Iterator[None]:
    outcome = yield
    if call.when != "call":
        return
    tape: Optional[TapeTest] = getattr(item, _KEY, None)
    if tape is None or tape.replay is None:
        return
    report = outcome.get_result()
    # A test that skipped itself, or an expected failure (failed as xfail
    # reports it, or passed unexpectedly), stopped wherever it stopped: the
    # replay's remaining steps say nothing about it.
    if report.skipped or hasattr(report, "wasxfail"):
        return
    # Read now, while the test's replay is still live: the fixture's
    # teardown has not run yet.
    try:
        failure = tape.failure()
    except Exception as error:  # an unreachable server: say so, do not pass silently
        failure = f"the replay of {tape.run} could not be checked: {error}"
    if failure is None:
        return
    previous = report.longrepr
    report.outcome = "failed"
    report.longrepr = "traza tape: " + failure + (f"\n\n{previous}" if previous else "")
