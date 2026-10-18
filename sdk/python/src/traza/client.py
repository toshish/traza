"""The HTTP client for Traza's agent-native routes.

Standard library only: ``urllib.request`` for transport, ``json`` for bodies.
The client holds no connection pool and no state between calls beyond its
configuration, so one instance may be shared across threads.
"""

from __future__ import annotations

import asyncio
import functools
import http.client
import inspect
import json
import os
import socket
import urllib.error
import urllib.parse
import urllib.request
from typing import IO, TYPE_CHECKING, Any, Callable, Dict, Iterator, List, Mapping, Optional, Sequence, Tuple, TypeVar, Union

from . import __version__
from .errors import FuseTripped, TrazaError, error_from_response
from .models import (
    DeliveredNudge,
    Diff,
    EvidencePack,
    Forks,
    FuseEvent,
    Goldens,
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

if TYPE_CHECKING:  # pragma: no cover - typing only
    from .forking import Fork

__all__ = ["Client", "Guard", "EventStream", "FOLLOW_WAIT_MS"]

#: The span attribute, and annotation name, that marks a run as golden.
GOLDEN = "traza.golden"

# Distinguishes "no tool result edit" from an edit whose result is None.
_NOT_GIVEN: Any = object()

F = TypeVar("F", bound=Callable[..., Any])

LimitsLike = Union[Limits, Mapping[str, Any]]

#: How long each read of a following event stream is held open when the
#: caller names no wait. Under the server's 30 s ceiling, and under the 30 to
#: 60 s idle timeouts common in reverse proxies and load balancers, so a quiet
#: read is answered by Traza rather than cut off by something in between.
FOLLOW_WAIT_MS = 25_000

_VERDICTS = ("continue", "warn", "stop")


class _RefuseRedirects(urllib.request.HTTPRedirectHandler):
    """Turns every 3xx into an error instead of following it.

    urllib copies a request's headers onto the redirected one, Authorization
    included: to another host, and from https to plain http. Traza never
    redirects its own routes, so a 3xx comes from something in front of it (a
    proxy canonicalising a host or upgrading a scheme), and the token must not
    follow it anywhere. Declining here hands the 3xx to the default error
    handler, which raises it as an ``HTTPError`` for :meth:`Client._send`.
    """

    def redirect_request(
        self,
        req: urllib.request.Request,
        fp: IO[bytes],
        code: int,
        msg: str,
        headers: http.client.HTTPMessage,
        newurl: str,
    ) -> Optional[urllib.request.Request]:
        return None


class Client:
    """A client for one Traza server.

    ``url`` is the server's base URL (``http://127.0.0.1:8080``, or the
    prefix a reverse proxy serves it under). ``token`` is sent as
    ``Authorization: Bearer``; leave it out for an open server. A redirect is
    never followed, so the token goes only to ``url``. ``tenant`` scopes
    every call that takes one, for an unbound credential. A bound credential
    can leave it out (the server scopes it to its own tenant) or name its own
    tenant; naming any other, ``""`` included, is refused with
    :class:`~traza.AuthError`. ``""`` names the default tenant. ``timeout``
    is in seconds, per request; a long-poll adds its own wait on top.
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
        # Built once per client rather than per call: building one reads the
        # proxy settings, which on macOS asks the system configuration.
        self._opener = urllib.request.build_opener(_RefuseRedirects)

    @classmethod
    def from_env(cls, environ: Optional[Mapping[str, str]] = None) -> "Client":
        """A client from ``TRAZA_URL``, ``TRAZA_TOKEN`` and ``TRAZA_TENANT``.

        The same variables the Claude Code hook in ``examples/guard`` reads,
        read the way it reads them: a variable set to the empty string is
        unset. ``TRAZA_TENANT=`` left in a profile therefore does not name
        the default tenant, which a bound credential would be refused.
        ``TRAZA_TOKEN`` (singular) is a client's bearer token; it is not the
        server's ``TRAZA_TOKENS`` list.
        """
        env = os.environ if environ is None else environ
        return cls(
            url=env.get("TRAZA_URL") or "http://127.0.0.1:8080",
            token=env.get("TRAZA_TOKEN") or None,
            tenant=env.get("TRAZA_TENANT") or None,
        )

    def __repr__(self) -> str:
        # Never the token: a repr ends up in logs and tracebacks.
        return f"Client(url={self.url!r}, tenant={self.tenant!r}, token={'set' if self.token else None})"

    # ------------------------------------------------------------ identity

    def whoami(self) -> Identity:
        """What the calling credential is: scope, binding, operator or not."""
        return decode(Identity, self._object("GET", "/v1/whoami"))

    def usage(self) -> UsageReport:
        """Ingest counters, limits and quota state, per tenant.

        A bound credential gets its own row. An unbound one gets every tenant
        with counters or limits, or only the client's ``tenant`` when set.
        """
        return decode(UsageReport, self._object("GET", "/v1/usage", query=self._tenant_query()))

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
        Nothing about the run's usage changes. A ``stop`` is a normal answer
        here, not an exception; :meth:`guard` is the form that raises. An
        answer that names no verdict (``continue``, ``warn`` or ``stop``)
        raises :class:`~traza.TrazaError` rather than passing for
        ``continue``.

        This is a pre-call check, so it also delivers the run's pending
        nudges, in ``.nudges`` — once: they are marked delivered on the
        server whether or not the caller reads them. ``advice`` is for the
        agent to hear (``.advice``); a ``block`` makes the answer ``stop``
        with its message as the reason.
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
        return decode(Verdict, self._object("POST", "/v1/fuses/check", body=body, verdict=True))

    def guard(
        self,
        run: Union[str, Callable[..., str]],
        est_cost_usd: Optional[float] = None,
        est_tokens: Optional[int] = None,
        tool: Optional[str] = None,
        fail_open: bool = False,
        on_nudges: Optional[Callable[[List[DeliveredNudge]], None]] = None,
    ) -> "Guard":
        """A context manager and decorator that checks before its body runs.

        On entry (or on each call of a decorated function) it asks
        :meth:`check` and raises :class:`~traza.FuseTripped` on ``stop``, so
        the step never starts. ``warn`` and ``continue`` let it through; the
        verdict is on the guard as ``.verdict``.

        Each check delivers the run's pending nudges once (see
        :meth:`check`). ``on_nudges``, when given, is called with them
        before the step runs — or before :class:`~traza.FuseTripped` is
        raised for a ``block`` — so a decorated agent step can hand the
        advice to its model; otherwise they are on ``.verdict.nudges``.

        As a decorator ``run`` may be a callable, called with the decorated
        function's arguments to name the run — for a graph node whose run id
        lives in its state. With ``fail_open=True`` a check that cannot be
        answered (an unreachable server, a refused check, or an answer that
        is not a verdict, such as a proxy's HTML page) lets the step run and
        leaves the error on ``.error``; by default the error propagates,
        because silently running unguarded is a decision the caller should
        make.
        """
        return Guard(self, run, est_cost_usd, est_tokens, tool, fail_open, on_nudges)

    def run_verdict(self, run: str) -> Verdict:
        """The run's current verdict, usage and limits.

        The first question about a run rebuilds it from stored spans
        (``source == "rebuilt"``). A run no span carries is ``continue``.
        """
        return decode(Verdict, self._object("GET", "/v1/fuses/runs/" + _segment(run), query=self._tenant_query()))

    def runs(self, state: str = "active", limit: Optional[int] = None) -> List[Verdict]:
        """Runs the guard holds, most recently active first.

        ``state`` is ``active`` (all of them), ``warn`` or ``stop``. Answered
        from the server's memory alone; nothing is rebuilt.
        """
        query = self._tenant_query()
        query["state"] = state
        if limit is not None:
            query["limit"] = limit
        page = self._object("GET", "/v1/fuses/runs", query=query)
        return [decode(Verdict, item) for item in page.get("runs", [])]

    def trip(self, run: str, reason: Optional[str] = None) -> Verdict:
        """The kill switch: stop ``run`` now, durably, until it is reset.

        ``reason`` is stored as given until the run's session or tenant is
        erased; keep personal data out of it. Needs an ``rw`` credential.
        """
        body = {"reason": reason} if reason is not None else {}
        return decode(
            Verdict,
            self._object("POST", "/v1/fuses/runs/" + _segment(run) + "/trip", query=self._tenant_query(), body=body),
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
            self._object("POST", "/v1/fuses/runs/" + _segment(run) + "/reset", query=self._tenant_query(), body=body),
        )

    def policies(self) -> Policies:
        """The stored fuse policies and the server's global default."""
        return decode(Policies, self._object("GET", "/v1/fuses/policies", query=self._tenant_query()))

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
        return decode(Policy, self._object("POST", "/v1/fuses/policies", body=body))

    def delete_policy(self, policy_id: str) -> Policy:
        """Delete a policy by id; returns the policy as it was."""
        answer = self._object("POST", "/v1/fuses/policies/" + _segment(policy_id) + "/delete")
        return decode(Policy, answer.get("policy") or {})

    def events(
        self, after: Optional[str] = None, wait_ms: Optional[int] = None, follow: bool = False
    ) -> "EventStream":
        """Verdict transitions, by cursor.

        Iterate the result for :class:`~traza.models.FuseEvent` objects.
        Iteration ends at the first read that returns nothing — with
        ``wait_ms=0`` (the default) that drains what the server retains; with
        a wait, after ``wait_ms`` without an event — unless ``follow`` is set,
        which reads forever. Every read of a following stream is a long-poll:
        ``wait_ms`` defaults to :data:`FOLLOW_WAIT_MS` (25 s), and
        ``follow=True`` with ``wait_ms=0`` is refused with ``ValueError``,
        because it would poll as fast as the network allows. The stream's
        ``cursor`` is the position to resume from with ``after=``, and
        ``gap`` turns true when the server could no longer honour a cursor
        (events may have been missed).
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
        return decode(Tape, self._object("GET", "/v1/tapes/" + _segment(run), query=query))

    def replay(
        self,
        run: str,
        mode: str = "strict",
        edits: Optional[Sequence[Mapping[str, Any]]] = None,
        live_after: Optional[int] = None,
    ) -> Replay:
        """Snapshot ``run`` into a replay that serves its model answers back.

        ``strict`` refuses a request whose messages differ from the recorded
        step (:class:`~traza.Conflict`, ``type == "replay_divergence"``, with
        the first difference in the error body); ``loose`` serves the
        recorded steps in order whatever is asked. The replay lives in server
        memory for an hour.

        ``edits`` and ``live_after`` make it a fork (see :meth:`fork`, which
        also switches the agent to the real provider at the cut): each edit
        is ``{"step": n, "response": ...}`` or ``{"step": n, "tool_result":
        ...}``, served instead of the recording at that step; after step
        ``live_after`` every call answers ``409 replay_live``.
        """
        body: Dict[str, Any] = {"run": run, "mode": mode}
        if edits:
            body["edits"] = [dict(edit) for edit in edits]
        if live_after is not None:
            body["live_after"] = live_after
        if self.tenant is not None:
            body["tenant"] = self.tenant
        replay = decode(Replay, self._object("POST", "/v1/replays", body=body))
        replay._client = self
        return replay

    def fork(
        self,
        run: str,
        at_step: int,
        response: Any = None,
        tool_result: Any = _NOT_GIVEN,
        *,
        live: bool = True,
        mode: str = "strict",
        **options: Any,
    ) -> "Fork":
        """Branch ``run`` at tape step ``at_step``: a context manager.

        Steps before ``at_step`` are replayed from the recording, strictly.
        At ``at_step`` the agent receives ``response`` (a model answer: text,
        or an assistant message) or ``tool_result`` (from
        :meth:`~traza.forking.Fork.tool_result`) instead of what was recorded —
        or, with neither, the recording stops there. With ``live`` (the
        default) every later call goes to the real provider: inside the
        ``with`` block ``OPENAI_BASE_URL`` and ``ANTHROPIC_BASE_URL`` point at
        a loopback switch that forwards to the replay until the cut and to
        the provider after it, and spans started in the block carry the
        fork's tags. See :class:`~traza.forking.Fork` for ``options``.
        """
        from .forking import Fork

        if tool_result is not _NOT_GIVEN:
            options["tool_result"] = tool_result
        return Fork(self, run, at_step, response=response, live=live, mode=mode, **options)

    def forks(self, run: str, limit: Optional[int] = None) -> Forks:
        """The recorded forks of ``run``, newest first, each diffed against it."""
        query = self._tenant_query()
        query["run"] = run
        if limit is not None:
            query["limit"] = limit
        return decode(Forks, self._object("GET", "/v1/forks", query=query))

    def goldens(self, limit: Optional[int] = None) -> Goldens:
        """The runs marked golden, by span attribute or by annotation."""
        query = self._tenant_query()
        if limit is not None:
            query["limit"] = limit
        return decode(Goldens, self._object("GET", "/v1/goldens", query=query))

    def mark_golden(self, run: str, golden: bool = True, source: str = "human:traza-sdk") -> None:
        """Mark ``run`` as a regression baseline, or with ``golden=False``
        un-mark it: an annotation named ``traza.golden`` on the run's session
        (or, for ``trace:<id>``, its trace). The newest one decides. Needs an
        ``rw`` credential."""
        if not isinstance(run, str) or not run:
            raise ValueError("expected a non-empty run id")
        body: Dict[str, Any] = {"name": GOLDEN, "value": bool(golden), "source": source}
        if run.startswith("trace:"):
            body["trace_id"] = run[len("trace:"):]
        else:
            body["session_id"] = run
        if self.tenant is not None:
            body["tenant"] = self.tenant
        self._object("POST", "/v1/annotations", body=body)

    def diff(self, a: str, b: str) -> Diff:
        """Align two runs' steps and name the first place they diverge."""
        query = self._tenant_query()
        query.update({"a": a, "b": b})
        return decode(Diff, self._object("GET", "/v1/diff", query=query))

    def recall(self, query: str, outcome: str = "any", limit: int = 5) -> Recall:
        """Earlier runs whose recorded text holds every word of ``query``.

        Successful runs rank first under ``outcome="any"``; ``success`` or
        ``failure`` keeps only runs that ended that way.
        """
        params = self._tenant_query()
        params.update({"q": query, "outcome": outcome, "limit": limit})
        return decode(Recall, self._object("GET", "/v1/recall", query=params))

    # ------------------------------------------------------------ ledger

    def ledger_head(self) -> LedgerHead:
        """The ledger's tip: what to anchor with an external notary."""
        return decode(LedgerHead, self._object("GET", "/v1/ledger/head"))

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
        status, text = self._send("GET", "/v1/evidence", query, None, self.timeout)
        pack = decode(EvidencePack, self._as_object(status, text))
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
        credential, timeout and error mapping as every method above.
        ``path`` is appended to the client's URL and must start with ``/``:
        any other path would continue the URL's host (``.attacker.example``
        names another server), so it raises ``ValueError`` before anything
        is sent, as does a request URL whose scheme, host or port is not the
        client's. ``query`` values of ``None`` are left out. A success whose
        body is not JSON raises :class:`~traza.TrazaError`.
        """
        status, text = self._send(method, path, query, body, self.timeout if timeout is None else timeout)
        return self._parse(status, text)

    def _object(
        self,
        method: str,
        path: str,
        query: Optional[Mapping[str, Any]] = None,
        body: Any = None,
        timeout: Optional[float] = None,
        verdict: bool = False,
    ) -> Dict[str, Any]:
        """:meth:`request` for a route that answers a JSON object.

        With ``verdict`` the object must also name a verdict the guard knows:
        an answer it cannot act on must not read as ``continue``.
        """
        status, text = self._send(method, path, query, body, self.timeout if timeout is None else timeout)
        return self._as_object(status, text, verdict)

    def _as_object(self, status: int, text: str, verdict: bool = False) -> Dict[str, Any]:
        value = self._parse(status, text)
        if not isinstance(value, dict):
            raise self._unreadable(status, text, "expected a JSON object, got " + _json_kind(value))
        if verdict and value.get("verdict") not in _VERDICTS:
            raise self._unreadable(status, text, "it names no verdict (continue, warn or stop)")
        return value

    def _parse(self, status: int, text: str) -> Any:
        if not text:
            return None
        try:
            return json.loads(text)
        except ValueError as error:
            raise self._unreadable(status, text, "not JSON") from error

    def _unreadable(self, status: Optional[int], body: Any, why: str) -> TrazaError:
        # A reverse proxy's sign-in or maintenance page, a captive portal, a
        # body cut short: an answer arrived, but not one Traza gave. It is a
        # TrazaError like a refused or unreachable request, so a fail-open
        # guard and code that rides out an unavailable server treat it alike
        # instead of meeting an exception they never expected.
        return TrazaError(f"unreadable answer from {self.url}: {why}", status=status, body=body)

    def _send(
        self,
        method: str,
        path: str,
        query: Optional[Mapping[str, Any]],
        body: Any,
        timeout: float,
    ) -> Tuple[int, str]:
        # Appended, not resolved, so a reverse proxy's path prefix on the
        # client's URL is kept. That makes a path not starting with "/" a
        # continuation of the host: "https://traza.example" and
        # ".attacker.example/v1/whoami" name another host, which the token
        # would reach with no redirect involved. Both checks come before the
        # token is attached; the second holds whatever builds the path.
        if not path.startswith("/"):
            raise ValueError(f'path must start with "/", got {path!r}')
        url = self.url + path
        if query:
            pairs = [(key, _query_value(value)) for key, value in query.items() if value is not None]
            if pairs:
                url += "?" + urllib.parse.urlencode(pairs)
        target, base = urllib.parse.urlsplit(url), urllib.parse.urlsplit(self.url)
        if (target.scheme, target.netloc) != (base.scheme, base.netloc):
            raise ValueError(f"{url!r} leaves the client's origin {base.scheme}://{base.netloc}")
        headers = {"Accept": "application/json", "User-Agent": f"traza-python/{__version__}"}
        data = None
        if body is not None:
            data = json.dumps(body, separators=(",", ":")).encode("utf-8")
            headers["Content-Type"] = "application/json"
        if self.token:
            headers["Authorization"] = "Bearer " + self.token
        req = urllib.request.Request(url, data=data, method=method, headers=headers)
        status = 0  # until a status line has been read
        try:
            with self._opener.open(req, timeout=timeout) as response:
                status = int(response.status)
                raw: bytes = response.read()
        except urllib.error.HTTPError as error:
            names = {name.lower(): value for name, value in error.headers.items()}
            if 300 <= error.code < 400:
                error.close()
                location = names.get("location", "an unnamed location")
                raise TrazaError(
                    f"{self.url} answered {error.code}, a redirect to {location}; the client follows no "
                    "redirect, so its token goes only to the URL it was given: configure the final URL",
                    status=error.code,
                    headers=names,
                ) from None
            try:
                raw = error.read() if error.fp is not None else b""
            except (OSError, http.client.HTTPException):
                raw = b""
            raise error_from_response(error.code, error.reason or "", names, raw) from None
        except urllib.error.URLError as error:
            raise TrazaError(f"could not reach {self.url}: {error.reason}") from error
        except (socket.timeout, TimeoutError) as error:
            raise TrazaError(f"no answer from {self.url} within {timeout} s") from error
        except http.client.HTTPException as error:
            # A status line that is not HTTP, a body shorter than its
            # Content-Length, an over-long header. Not OSError, so without
            # this arm it would escape as a type no caller expects.
            raise self._unreadable(status or None, None, f"{type(error).__name__}: {error}") from error
        except OSError as error:
            # A connection reset mid-response, after urlopen returned.
            raise TrazaError(f"connection to {self.url} failed: {error}") from error
        try:
            return status, raw.decode("utf-8")
        except UnicodeDecodeError as error:
            raise self._unreadable(status, raw, "not UTF-8") from error

    def _tenant_query(self) -> Dict[str, Any]:
        return {"tenant": self.tenant} if self.tenant is not None else {}

    # Called by Replay, which holds a reference to the client that made it.

    def _replay_status(self, replay_id: str) -> ReplayStatus:
        return decode(ReplayStatus, self._object("GET", "/v1/replays/" + _segment(replay_id)))

    def _replay_next_tool(self, replay_id: str, name: Optional[str]) -> Optional[ToolStep]:
        body = {"tool": name} if name is not None else {}
        try:
            answer = self._object("POST", "/v1/replays/" + _segment(replay_id) + "/tools/next", body=body)
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
        on_nudges: Optional[Callable[[List[DeliveredNudge]], None]] = None,
    ) -> None:
        self.client = client
        self.run = run
        self.est_cost_usd = est_cost_usd
        self.est_tokens = est_tokens
        self.tool = tool
        self.fail_open = fail_open
        self.on_nudges = on_nudges
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
        if verdict.nudges and self.on_nudges is not None:
            self.on_nudges(verdict.nudges)
        if verdict.stopped:
            raise FuseTripped(verdict)
        return verdict

    def __enter__(self) -> "Guard":
        self.check()
        return self

    def __exit__(self, *exc_info: Any) -> None:
        return None

    async def __aenter__(self) -> "Guard":
        # urllib blocks; a worker thread keeps the event loop serving.
        await asyncio.get_running_loop().run_in_executor(None, self.check)
        return self

    async def __aexit__(self, *exc_info: Any) -> None:
        return None

    def __call__(self, fn: F) -> F:
        def run_for(args: Tuple[Any, ...], kwargs: Dict[str, Any]) -> str:
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

    def __init__(self, client: Client, after: Optional[str], wait_ms: Optional[int], follow: bool) -> None:
        if wait_ms is None:
            wait_ms = FOLLOW_WAIT_MS if follow else 0
        if not 0 <= wait_ms <= 30000:
            raise ValueError("wait_ms must be between 0 and 30000")
        if follow and wait_ms == 0:
            # Every empty read would be followed at once by the next one:
            # thousands of requests a second, each on a new connection.
            raise ValueError("follow needs a long-poll: give wait_ms > 0, or leave it out for 25000")
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
            page = self.client._object("GET", "/v1/fuses/events", query=query, timeout=timeout)
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


def _json_kind(value: Any) -> str:
    if value is None:
        return "nothing"
    if isinstance(value, list):
        return "an array"
    if isinstance(value, str):
        return "a string"
    if isinstance(value, bool):
        return "a boolean"
    return "a number"


def _query_value(value: Any) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    return str(value)


def _limits(value: LimitsLike) -> Dict[str, Any]:
    if isinstance(value, Limits):
        return value.to_dict()
    return {key: item for key, item in dict(value).items() if item is not None}
