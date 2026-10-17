"""The HTTP client for Traza's agent-native routes.

Standard library only: ``urllib.request`` for transport, ``json`` for bodies.
The client holds no connection pool and no state between calls beyond its
configuration, so one instance may be shared across threads.
"""

from __future__ import annotations

import asyncio
import functools
import inspect
import json
import os
import socket
import urllib.error
import urllib.parse
import urllib.request
from typing import Any, Callable, Dict, Iterator, List, Mapping, Optional, TypeVar, Union

from . import __version__
from .errors import FuseTripped, TrazaError, error_from_response
from .models import (
    Diff,
    EvidencePack,
    FuseEvent,
    Identity,
    LedgerHead,
    Limits,
    Policies,
    Policy,
    Recall,
    Replay,
    ReplayStatus,
    Tape,
    ToolStep,
    UsageReport,
    Verdict,
    decode,
)

__all__ = ["Client", "Guard", "EventStream"]

F = TypeVar("F", bound=Callable[..., Any])

LimitsLike = Union[Limits, Mapping[str, Any]]


class Client:
    """A client for one Traza server.

    ``url`` is the server's base URL (``http://127.0.0.1:8080``, or the
    prefix a reverse proxy serves it under). ``token`` is sent as
    ``Authorization: Bearer``; leave it out for an open server. ``tenant``
    scopes every call that takes one, for an unbound credential; a bound
    credential is held to its own tenant by the server whatever is sent, and
    naming another is refused with :class:`~traza.AuthError`. ``""`` names
    the default tenant. ``timeout`` is in seconds, per request; a long-poll
    adds its own wait on top.
    """

    def __init__(
        self,
        url: str = "http://127.0.0.1:8080",
        token: Optional[str] = None,
        tenant: Optional[str] = None,
        timeout: float = 10,
    ) -> None:
        parsed = urllib.parse.urlsplit(url)
        if parsed.scheme not in ("http", "https") or not parsed.netloc:
            raise ValueError(f"url must be an http:// or https:// URL, got {url!r}")
        self.url = url.rstrip("/")
        self.token = token or None
        self.tenant = tenant
        self.timeout = timeout

    @classmethod
    def from_env(cls, environ: Optional[Mapping[str, str]] = None) -> "Client":
        """A client from ``TRAZA_URL``, ``TRAZA_TOKEN`` and ``TRAZA_TENANT``.

        The same variables the Claude Code hook in ``examples/guard`` reads.
        ``TRAZA_TOKEN`` (singular) is a client's bearer token; it is not the
        server's ``TRAZA_TOKENS`` list.
        """
        env = os.environ if environ is None else environ
        return cls(
            url=env.get("TRAZA_URL") or "http://127.0.0.1:8080",
            token=env.get("TRAZA_TOKEN") or None,
            tenant=env.get("TRAZA_TENANT"),
        )

    def __repr__(self) -> str:
        # Never the token: a repr ends up in logs and tracebacks.
        return f"Client(url={self.url!r}, tenant={self.tenant!r}, token={'set' if self.token else None})"

    # ------------------------------------------------------------ identity

    def whoami(self) -> Identity:
        """What the calling credential is: scope, binding, operator or not."""
        return decode(Identity, self.request("GET", "/v1/whoami"))

    def usage(self) -> UsageReport:
        """Ingest counters, limits and quota state, per tenant.

        A bound credential gets its own row. An unbound one gets every tenant
        with counters or limits, or only the client's ``tenant`` when set.
        """
        return decode(UsageReport, self.request("GET", "/v1/usage", query=self._tenant_query()))

    # ------------------------------------------------------------ fuses

    def check(
        self,
        run: str,
        est_cost_usd: Optional[float] = None,
        est_tokens: Optional[int] = None,
        tool: Optional[str] = None,
    ) -> Verdict:
        """The pre-call gate: may ``run`` take its next step?

        The next step — an LLM call, or a call of ``tool`` when named — is
        added to the run's recorded usage hypothetically, with the estimates,
        and a step that would go past a ``stop`` limit answers ``stop``.
        Nothing about the run changes. A ``stop`` is a normal answer here,
        not an exception; :meth:`guard` is the form that raises.
        """
        nxt: Dict[str, Any] = {}
        if est_cost_usd is not None:
            nxt["est_cost_usd"] = est_cost_usd
        if est_tokens is not None:
            nxt["est_tokens"] = est_tokens
        if tool is not None:
            nxt["tool"] = tool
        body: Dict[str, Any] = {"run": run, "next": nxt}
        if self.tenant is not None:
            body["tenant"] = self.tenant
        return decode(Verdict, self.request("POST", "/v1/fuses/check", body=body))

    def guard(
        self,
        run: Union[str, Callable[..., str]],
        est_cost_usd: Optional[float] = None,
        est_tokens: Optional[int] = None,
        tool: Optional[str] = None,
        fail_open: bool = False,
    ) -> "Guard":
        """A context manager and decorator that checks before its body runs.

        On entry (or on each call of a decorated function) it asks
        :meth:`check` and raises :class:`~traza.FuseTripped` on ``stop``, so
        the step never starts. ``warn`` and ``continue`` let it through; the
        verdict is on the guard as ``.verdict``.

        As a decorator ``run`` may be a callable, called with the decorated
        function's arguments to name the run — for a graph node whose run id
        lives in its state. With ``fail_open=True`` a server that cannot be
        reached (or refuses the check) lets the step run and leaves the error
        on ``.error``; by default the error propagates, because silently
        running unguarded is a decision the caller should make.
        """
        return Guard(self, run, est_cost_usd, est_tokens, tool, fail_open)

    def run_verdict(self, run: str) -> Verdict:
        """The run's current verdict, usage and limits.

        The first question about a run rebuilds it from stored spans
        (``source == "rebuilt"``). A run no span carries is ``continue``.
        """
        return decode(Verdict, self.request("GET", "/v1/fuses/runs/" + _segment(run), query=self._tenant_query()))

    def runs(self, state: str = "active", limit: Optional[int] = None) -> List[Verdict]:
        """Runs the guard holds, most recently active first.

        ``state`` is ``active`` (all of them), ``warn`` or ``stop``. Answered
        from the server's memory alone; nothing is rebuilt.
        """
        query = self._tenant_query()
        query["state"] = state
        if limit is not None:
            query["limit"] = limit
        page = self.request("GET", "/v1/fuses/runs", query=query)
        return [decode(Verdict, item) for item in page.get("runs", [])]

    def trip(self, run: str, reason: Optional[str] = None) -> Verdict:
        """The kill switch: stop ``run`` now, durably, until it is reset.

        ``reason`` is stored as given until the run's session or tenant is
        erased; keep personal data out of it. Needs an ``rw`` credential.
        """
        body = {"reason": reason} if reason is not None else {}
        return decode(
            Verdict,
            self.request("POST", "/v1/fuses/runs/" + _segment(run) + "/trip", query=self._tenant_query(), body=body),
        )

    def reset(self, run: str, grant: Optional[LimitsLike] = None) -> Verdict:
        """Clear a stop (a policy's or the kill switch), the error streak and
        the repeat counts.

        Cumulative usage is not refilled, so a run still over a cumulative
        limit answers ``stop`` again at once. ``grant`` raises the run's
        limits to the absolute values named, over its policy's.
        """
        body = {"grant": _limits(grant)} if grant is not None else {}
        return decode(
            Verdict,
            self.request("POST", "/v1/fuses/runs/" + _segment(run) + "/reset", query=self._tenant_query(), body=body),
        )

    def policies(self) -> Policies:
        """The stored fuse policies and the server's global default."""
        return decode(Policies, self.request("GET", "/v1/fuses/policies", query=self._tenant_query()))

    def put_policy(
        self,
        name: str,
        limits: LimitsLike,
        service: Optional[str] = None,
        action: str = "warn",
    ) -> Policy:
        """Create a policy, or replace the tenant's policy of the same name.

        ``service`` matches runs whose first span came from that service, and
        is required unless ``name`` is ``default`` (the tenant's fallback,
        which must not have one). ``action`` is ``stop`` or ``warn``. Limits
        are not inherited from ``default`` or ``global``.
        """
        body: Dict[str, Any] = {"name": name, "limits": _limits(limits), "action": action}
        if service is not None:
            body["match"] = {"service": service}
        if self.tenant is not None:
            body["tenant"] = self.tenant
        return decode(Policy, self.request("POST", "/v1/fuses/policies", body=body))

    def delete_policy(self, policy_id: str) -> Policy:
        """Delete a policy by id; returns the policy as it was."""
        answer = self.request("POST", "/v1/fuses/policies/" + _segment(policy_id) + "/delete")
        return decode(Policy, answer.get("policy") or {})

    def events(self, after: Optional[str] = None, wait_ms: int = 0, follow: bool = False) -> "EventStream":
        """Verdict transitions, by cursor.

        Iterate the result for :class:`~traza.models.FuseEvent` objects.
        Iteration ends at the first read that returns nothing — with
        ``wait_ms=0`` that drains what the server retains; with a wait, after
        ``wait_ms`` without an event — unless ``follow`` is set, which polls
        forever. The stream's ``cursor`` is the position to resume from with
        ``after=``, and ``gap`` turns true when the server could no longer
        honour a cursor (events may have been missed).
        """
        return EventStream(self, after, wait_ms, follow)

    # ------------------------------------------------------------ tapes and replays

    def tape(self, run: str, resolve: bool = False, max_steps: Optional[int] = None) -> Tape:
        """The run's steps — model, tool and agent calls — in start order.

        ``resolve`` inlines offloaded ``$payload`` values, up to the server's
        ``--tape-max-bytes``.
        """
        query = self._tenant_query()
        if resolve:
            query["resolve"] = "true"
        if max_steps is not None:
            query["max_steps"] = max_steps
        return decode(Tape, self.request("GET", "/v1/tapes/" + _segment(run), query=query))

    def replay(self, run: str, mode: str = "strict") -> Replay:
        """Snapshot ``run`` into a replay that serves its model answers back.

        ``strict`` refuses a request whose messages differ from the recorded
        step (:class:`~traza.Conflict`, ``type == "replay_divergence"``);
        ``loose`` serves the recorded steps in order whatever is asked. The
        replay lives in server memory for an hour.
        """
        body: Dict[str, Any] = {"run": run, "mode": mode}
        if self.tenant is not None:
            body["tenant"] = self.tenant
        replay = decode(Replay, self.request("POST", "/v1/replays", body=body))
        replay._client = self
        return replay

    def diff(self, a: str, b: str) -> Diff:
        """Align two runs' steps and name the first place they diverge."""
        query = self._tenant_query()
        query.update({"a": a, "b": b})
        return decode(Diff, self.request("GET", "/v1/diff", query=query))

    def recall(self, query: str, outcome: str = "any", limit: int = 5) -> Recall:
        """Earlier runs whose recorded text holds every word of ``query``.

        Successful runs rank first under ``outcome="any"``; ``success`` or
        ``failure`` keeps only runs that ended that way.
        """
        params = self._tenant_query()
        params.update({"q": query, "outcome": outcome, "limit": limit})
        return decode(Recall, self.request("GET", "/v1/recall", query=params))

    # ------------------------------------------------------------ ledger

    def ledger_head(self) -> LedgerHead:
        """The ledger's tip: what to anchor with an external notary."""
        return decode(LedgerHead, self.request("GET", "/v1/ledger/head"))

    def evidence(self, since: Optional[int] = None, until: Optional[int] = None) -> EvidencePack:
        """The evidence pack for a window of Unix nanoseconds.

        A bound credential gets its tenant's pack. An unbound one gets the
        client's ``tenant`` when set, else the whole store.
        """
        query = self._tenant_query()
        if since is not None:
            query["since"] = since
        if until is not None:
            query["until"] = until
        text = self._send("GET", "/v1/evidence", query, None, self.timeout)
        pack = decode(EvidencePack, json.loads(text))
        pack._text = text
        return pack

    # ------------------------------------------------------------ transport

    def request(
        self,
        method: str,
        path: str,
        query: Optional[Mapping[str, Any]] = None,
        body: Any = None,
        timeout: Optional[float] = None,
    ) -> Any:
        """Send one request and return its parsed JSON body.

        The escape hatch for routes this client does not wrap: the same
        credential, timeout and error mapping as every method above. ``path``
        starts with ``/``; ``query`` values of ``None`` are left out.
        """
        text = self._send(method, path, query, body, self.timeout if timeout is None else timeout)
        return json.loads(text) if text else None

    def _send(
        self,
        method: str,
        path: str,
        query: Optional[Mapping[str, Any]],
        body: Any,
        timeout: float,
    ) -> str:
        url = self.url + path
        if query:
            pairs = [(key, _query_value(value)) for key, value in query.items() if value is not None]
            if pairs:
                url += "?" + urllib.parse.urlencode(pairs)
        headers = {"Accept": "application/json", "User-Agent": f"traza-python/{__version__}"}
        data = None
        if body is not None:
            data = json.dumps(body, separators=(",", ":")).encode("utf-8")
            headers["Content-Type"] = "application/json"
        if self.token:
            headers["Authorization"] = "Bearer " + self.token
        req = urllib.request.Request(url, data=data, method=method, headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=timeout) as response:
                return response.read().decode("utf-8")
        except urllib.error.HTTPError as error:
            raw = error.read() if error.fp is not None else b""
            raise error_from_response(error.code, error.reason or "", dict(error.headers.items()), raw) from None
        except urllib.error.URLError as error:
            raise TrazaError(f"could not reach {self.url}: {error.reason}") from error
        except (socket.timeout, TimeoutError) as error:
            raise TrazaError(f"no answer from {self.url} within {timeout} s") from error
        except OSError as error:
            # A connection reset mid-response, after urlopen returned.
            raise TrazaError(f"connection to {self.url} failed: {error}") from error

    def _tenant_query(self) -> Dict[str, Any]:
        return {"tenant": self.tenant} if self.tenant is not None else {}

    # Called by Replay, which holds a reference to the client that made it.

    def _replay_status(self, replay_id: str) -> ReplayStatus:
        return decode(ReplayStatus, self.request("GET", "/v1/replays/" + _segment(replay_id)))

    def _replay_next_tool(self, replay_id: str, name: Optional[str]) -> Optional[ToolStep]:
        body = {"tool": name} if name is not None else {}
        try:
            answer = self.request("POST", "/v1/replays/" + _segment(replay_id) + "/tools/next", body=body)
        except TrazaError as error:
            # Running out of recorded tool steps is how a harness's loop ends,
            # not a failure; a withdrawn replay (also 410) still raises.
            if error.status == 410 and error.type == "replay_exhausted":
                return None
            raise
        return decode(ToolStep, answer)


class Guard:
    """The guard :meth:`Client.guard` returns. See there."""

    def __init__(
        self,
        client: Client,
        run: Union[str, Callable[..., str]],
        est_cost_usd: Optional[float],
        est_tokens: Optional[int],
        tool: Optional[str],
        fail_open: bool,
    ) -> None:
        self.client = client
        self.run = run
        self.est_cost_usd = est_cost_usd
        self.est_tokens = est_tokens
        self.tool = tool
        self.fail_open = fail_open
        #: The last verdict this guard received; ``None`` before the first
        #: check, or when a fail-open check could not be answered.
        self.verdict: Optional[Verdict] = None
        #: The error a fail-open check swallowed, if any.
        self.error: Optional[TrazaError] = None

    def check(self, run: Optional[str] = None) -> Optional[Verdict]:
        """Ask the gate now; raise :class:`~traza.FuseTripped` on ``stop``."""
        if run is None:
            if callable(self.run):
                raise TypeError("this guard names its run with a callable; use it as a decorator")
            run = self.run
        self.error = None
        try:
            verdict = self.client.check(run, self.est_cost_usd, self.est_tokens, self.tool)
        except TrazaError as error:
            if not self.fail_open:
                raise
            self.verdict, self.error = None, error
            return None
        self.verdict = verdict
        if verdict.stopped:
            raise FuseTripped(verdict)
        return verdict

    def __enter__(self) -> "Guard":
        self.check()
        return self

    def __exit__(self, *exc_info: Any) -> bool:
        return False

    async def __aenter__(self) -> "Guard":
        # urllib blocks; a worker thread keeps the event loop serving.
        await asyncio.get_running_loop().run_in_executor(None, self.check)
        return self

    async def __aexit__(self, *exc_info: Any) -> bool:
        return False

    def __call__(self, fn: F) -> F:
        def run_for(args: tuple, kwargs: dict) -> str:
            return self.run(*args, **kwargs) if callable(self.run) else self.run

        if inspect.iscoroutinefunction(fn):

            @functools.wraps(fn)
            async def async_wrapper(*args: Any, **kwargs: Any) -> Any:
                run = run_for(args, kwargs)
                await asyncio.get_running_loop().run_in_executor(None, self.check, run)
                return await fn(*args, **kwargs)

            return async_wrapper  # type: ignore[return-value]

        @functools.wraps(fn)
        def wrapper(*args: Any, **kwargs: Any) -> Any:
            self.check(run_for(args, kwargs))
            return fn(*args, **kwargs)

        return wrapper  # type: ignore[return-value]


class EventStream:
    """An iterable over fuse events. See :meth:`Client.events`."""

    def __init__(self, client: Client, after: Optional[str], wait_ms: int, follow: bool) -> None:
        if not 0 <= wait_ms <= 30000:
            raise ValueError("wait_ms must be between 0 and 30000")
        self.client = client
        #: Where the next read starts; pass it back as ``after=`` to resume.
        self.cursor = after
        self.wait_ms = wait_ms
        self.follow = follow
        #: True once the server answered that it could not honour a cursor.
        self.gap = False

    def __iter__(self) -> Iterator[FuseEvent]:
        while True:
            query = self.client._tenant_query()
            query["wait_ms"] = self.wait_ms
            if self.cursor is not None:
                query["after"] = self.cursor
            # The server holds a long-poll open for up to wait_ms; the socket
            # timeout must outlast it or every quiet poll would read as an
            # unreachable server.
            timeout = self.client.timeout + self.wait_ms / 1000
            page = self.client.request("GET", "/v1/fuses/events", query=query, timeout=timeout)
            if page.get("gap"):
                self.gap = True
            events = page.get("events") or []
            for item in events:
                event = decode(FuseEvent, item)
                # Advanced before the yield, so a consumer that stops early
                # resumes just past the last event it was handed.
                if event.cursor is not None:
                    self.cursor = event.cursor
                yield event
            # `next` can be past the last event: it also skips events a
            # tenant filter hid, so a filtered reader keeps pace.
            if page.get("next") is not None:
                self.cursor = page["next"]
            if not events and not self.follow:
                return


def _segment(value: str) -> str:
    """Percent-encode one path segment: a run id may contain ``/``."""
    if not isinstance(value, str) or not value:
        raise ValueError("expected a non-empty string")
    return urllib.parse.quote(value, safe="")


def _query_value(value: Any) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    return str(value)


def _limits(value: LimitsLike) -> Dict[str, Any]:
    if isinstance(value, Limits):
        return value.to_dict()
    return {key: item for key, item in dict(value).items() if item is not None}
