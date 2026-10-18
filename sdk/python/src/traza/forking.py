"""Fork & Rewind: branch a recorded run at any step, and let the agent go on.

A fork replays a recorded run up to a step, serves a different answer there
(or simply stops serving the recording), and hands every later call to the
real provider — so the agent continues, for real, down a branch the
recording never took::

    import traza

    with traza.fork("weather-7", at_step=2, response="It is sunny in Paris.") as fork:
        run_agent("What's the weather in Paris?")   # steps 0-1 replayed, 2 edited, then live
    print(fork.id, fork.status().fork.live_calls)

Inside the ``with`` block ``OPENAI_BASE_URL`` and ``ANTHROPIC_BASE_URL``
point at a *switch*: a small HTTP server on a loopback port, private to this
process, that forwards each model call to the Traza replay until the replay
answers ``409 replay_live``, and from then on to the real provider (the base
URL and key the environment named when the fork was made). The agent sees
one provider throughout. Only a model call's answer moves the switch: the
server keeps the tool and model cursors apart, so a tool the recording does
not hold (:meth:`Fork.tool_result` answering ``None``) runs for real while
the recorded model calls are still served from the recording. An SDK client
constructed *before* the block keeps the URL it was built with: build it
inside, or pass :meth:`Fork.openai_kwargs`.

The environment is the whole process's, so only one fork at a time may own
it: starting a second ``set_env`` fork while one is active is refused. Forks
that run side by side — nested, or one per thread for parallel what-ifs —
take ``set_env=False`` and hand each agent its own :meth:`Fork.openai_kwargs`
(or :meth:`Fork.env`, for a subprocess). A fork never mistakes another
fork's switch for the real provider: the live provider is read from the
environment as it stood before any fork changed it.

Traza itself never calls a model. The switch is the agent's own traffic, in
the agent's own process; it listens on ``127.0.0.1`` only, and answers only
requests that present the per-fork secret it put in ``OPENAI_API_KEY`` /
``ANTHROPIC_API_KEY``, so nothing else on the machine can borrow the real key
it forwards with.

Spans started inside the block carry the fork's tags (``traza.fork.parent``,
``traza.fork.step``, ``traza.fork.id``) and the fork's own session id, when
the OpenTelemetry SDK is installed and configured before the fork is entered:
a span processor is added to the global tracer provider once, and stamps the
fork active in the span's context (see :class:`ForkSpanProcessor`). That is
what lets ``GET /v1/forks`` find the branch again and diff it against its
parent. Threads do not inherit context variables, so a span started on a
worker thread is tagged only when that thread runs in a copy of the block's
context (``contextvars.copy_context().run``) — or, for a process that runs
nothing but the forked agent, with ``tag_threads=True``.

Standard library only; OpenTelemetry is used when present, never required.
"""

from __future__ import annotations

import contextvars
import hmac
import http.client
import json
import os
import secrets
import socketserver
import ssl
import threading
import time
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import TYPE_CHECKING, Any, Callable, Dict, List, Mapping, Optional, Tuple

from .errors import TrazaError
from .models import Diff, Replay, ReplayStatus, ToolStep

if TYPE_CHECKING:  # pragma: no cover - typing only
    from .client import Client

__all__ = [
    "Fork",
    "ForkSpanProcessor",
    "active_fork",
    "install_span_tags",
    "FORK_PARENT",
    "FORK_STEP",
    "FORK_ID",
]

#: The span attribute naming the run a forked run branched from.
FORK_PARENT = "traza.fork.parent"
#: The span attribute naming the step the fork was cut at.
FORK_STEP = "traza.fork.step"
#: The span attribute carrying the fork's id.
FORK_ID = "traza.fork.id"

DEFAULT_OPENAI_BASE_URL = "https://api.openai.com/v1"
DEFAULT_ANTHROPIC_BASE_URL = "https://api.anthropic.com"

#: How long the switch waits for the real provider, per call.
LIVE_TIMEOUT_S = 600.0

# Headers that describe one hop of a connection, never the request itself,
# plus the ones the switch sets itself (on the way out: credentials and the
# body's framing; on the way back: its own Server and Date).
_HOP_BY_HOP = frozenset(
    [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "proxy-connection",
        "te",
        "trailer",
        "trailers",
        "transfer-encoding",
        "upgrade",
        "host",
        "content-length",
        "authorization",
        "x-api-key",
        "accept-encoding",
        "server",
        "date",
    ]
)

_UNSET: Any = object()

_ACTIVE: "contextvars.ContextVar[Optional[Fork]]" = contextvars.ContextVar("traza_fork", default=None)
_LOCK = threading.Lock()
_TAGGED_PROVIDERS: List[Any] = []
#: The fork whose block has pointed the process environment at its switch.
_ENV_OWNER: Optional["Fork"] = None
#: The fork tagging spans from contexts that carry none (``tag_threads``).
_THREAD_TAGGER: Optional["Fork"] = None
#: The URLs of the switches listening in this process.
_SWITCHES: Dict[str, "Fork"] = {}

_SECRET_PREFIX = "traza-fork-"


def active_fork() -> Optional["Fork"]:
    """The fork whose ``with`` block the caller is in — this context's —
    else the one fork started with ``tag_threads=True``, if any. A thread
    started inside a block does not inherit its context variables, so its
    spans are not the fork's unless it runs in a copy of the block's context
    or that fork tags every thread."""
    fork = _ACTIVE.get()
    if fork is not None:
        return fork
    with _LOCK:
        return _THREAD_TAGGER


def _real_env(key: str) -> Optional[str]:
    """``os.environ[key]`` as it stood before any fork changed it: the value
    the environment-owning fork saved, when one is active — and never a
    fork's own switch URL or secret."""
    with _LOCK:
        owner = _ENV_OWNER
        if owner is not None and key in owner._saved_env:
            value = owner._saved_env[key]
        else:
            value = os.environ.get(key)
        switches = list(_SWITCHES)
    if not value:
        return None
    if value.startswith(_SECRET_PREFIX) or any(value.startswith(url) for url in switches):
        return None
    return value


class ForkSpanProcessor:
    """An OpenTelemetry span processor that stamps the fork active where a
    span starts (:func:`active_fork`) on that span.

    Duck-typed against ``opentelemetry.sdk.trace.SpanProcessor`` so that this
    package needs no dependency. Added once to the global tracer provider by
    :func:`install_span_tags`; outside a fork's context it does nothing, so a
    span another thread starts meanwhile — a web server's other request, say
    — keeps its own run unless the fork was made with ``tag_threads=True``.
    Attributes are
    set at start, so an instrumentation that sets ``session.id`` itself later
    in the span's life overrides the fork's session — give such an agent
    :attr:`Fork.run_id` as its session instead.
    """

    def on_start(self, span: Any, parent_context: Any = None) -> None:
        fork = active_fork()
        if fork is None:
            return
        for key, value in fork.span_attributes().items():
            span.set_attribute(key, value)

    def _on_ending(self, span: Any) -> None:
        return None

    def on_end(self, span: Any) -> None:
        return None

    def shutdown(self) -> None:
        return None

    def force_flush(self, timeout_millis: int = 30000) -> bool:
        return True


def install_span_tags() -> bool:
    """Adds a :class:`ForkSpanProcessor` to the global OpenTelemetry tracer
    provider, once per provider. Returns whether spans will be tagged: not
    when OpenTelemetry is not installed, or when no SDK tracer provider has
    been set yet (the API's proxy provider takes no processors)."""
    try:
        from opentelemetry import trace  # type: ignore[import-not-found]
    except ImportError:
        return False
    provider = trace.get_tracer_provider()
    add = getattr(provider, "add_span_processor", None)
    if add is None:
        return False
    with _LOCK:
        if any(seen is provider for seen in _TAGGED_PROVIDERS):
            return True
        add(ForkSpanProcessor())
        _TAGGED_PROVIDERS.append(provider)
    return True


class Fork:
    """A branch of a recorded run, made by :meth:`traza.Client.fork` (or
    :func:`traza.fork`). Use it as a context manager.

    ``at_step`` is a tape step of the parent run (``Client.tape``). With
    ``response`` (a model answer: text, an assistant message, or a list of
    them) or ``tool_result`` (any JSON), that step is served the edit instead
    of the recording; otherwise the recording simply stops before it. With
    ``live=True`` every call after the cut goes to the real provider; with
    ``live=False`` an edit is served and the recording then plays on, loosely
    (strict matching ends at the first edit either way).

    Options:

    - ``run_id`` — the session id the forked run's spans get (default: the
      fork id). ``tag_session=False`` leaves the agent's own.
    - ``live_openai_base_url`` / ``live_openai_api_key`` /
      ``live_anthropic_base_url`` / ``live_anthropic_api_key`` — where live
      calls go. Default: ``OPENAI_BASE_URL`` (else OpenAI's API) and
      ``OPENAI_API_KEY``, and the Anthropic equivalents, as the environment
      names them when the fork is made.
      That environment is read as it stood before any fork changed it,
      so it is never another fork's switch.
    - ``set_env=False`` — leave the process environment alone; use
      :meth:`env` or :meth:`openai_kwargs` to point the agent at the switch.
      Required of every fork but one when forks overlap: the environment is
      the process's, and a second ``set_env`` fork is refused while one is
      active.
    - ``tag_spans=False`` — do not add the span processor.
    - ``tag_threads=True`` — also tag spans started in contexts that carry
      no fork (threads the agent starts). Only for a process that runs
      nothing but the forked agent: every other thread's spans would join
      the fork's run too. At most one such fork at a time.
    """

    def __init__(
        self,
        client: "Client",
        run: str,
        at_step: int,
        response: Any = _UNSET,
        tool_result: Any = _UNSET,
        *,
        live: bool = True,
        mode: str = "strict",
        run_id: Optional[str] = None,
        tag_session: bool = True,
        live_openai_base_url: Optional[str] = None,
        live_openai_api_key: Optional[str] = None,
        live_anthropic_base_url: Optional[str] = None,
        live_anthropic_api_key: Optional[str] = None,
        set_env: bool = True,
        tag_spans: bool = True,
        tag_threads: bool = False,
    ) -> None:
        if not isinstance(run, str) or not run:
            raise ValueError("expected a non-empty run id")
        if isinstance(at_step, bool) or not isinstance(at_step, int) or at_step < 0:
            raise ValueError("at_step must be a non-negative integer: a step of the run's tape")
        # Client.fork passes None for an absent edit; a tool result of None
        # is still expressible directly here.
        response = _UNSET if response is None else response
        if response is not _UNSET and tool_result is not _UNSET:
            raise ValueError("an edit is a response or a tool_result, not both")
        if response is _UNSET and tool_result is _UNSET:
            if not live:
                raise ValueError("a fork needs an edit (response or tool_result), or live=True")
            if at_step == 0:
                raise ValueError(
                    "at_step=0 with no edit replays nothing: the run would be live from its first "
                    "call, which is just running the agent"
                )
        self.client = client
        self.parent = run
        self.at_step = at_step
        self.mode = mode
        self.live = live
        self._response = response
        self._tool_result = tool_result
        self._run_id = run_id
        self.tag_session = tag_session
        self.set_env = set_env
        self.tag_spans = tag_spans
        self.tag_threads = tag_threads
        self.live_openai_base_url = (
            live_openai_base_url or _real_env("OPENAI_BASE_URL") or DEFAULT_OPENAI_BASE_URL
        ).rstrip("/")
        self.live_openai_api_key = live_openai_api_key or _real_env("OPENAI_API_KEY")
        self.live_anthropic_base_url = (
            live_anthropic_base_url or _real_env("ANTHROPIC_BASE_URL") or DEFAULT_ANTHROPIC_BASE_URL
        ).rstrip("/")
        self.live_anthropic_api_key = live_anthropic_api_key or _real_env("ANTHROPIC_API_KEY")
        #: The replay serving the fork's beginning, once entered.
        self.replay: Optional[Replay] = None
        #: The fork id (``fk_…``), once entered.
        self.id: Optional[str] = None
        #: The step it was cut at, as the server computed it.
        self.step: Optional[int] = None
        #: Unix seconds when model calls went to the real provider: when the
        #: replay first answered a model call ``replay_live``. A tool step the
        #: replay answers that way (``tool_result`` returning ``None``) does
        #: not set it — tool and model calls have their own cursors.
        self.live_at: Optional[float] = None
        #: Whether spans are being tagged (OpenTelemetry SDK configured).
        self.tagging = False
        #: The replay's status as the block ended (best effort).
        self.final_status: Optional[ReplayStatus] = None
        self._switch: Optional[_Switch] = None
        self._saved_env: Dict[str, Optional[str]] = {}
        self._token: Optional[contextvars.Token] = None

    # ---------------------------------------------------------- the spec

    def edits(self) -> List[Dict[str, Any]]:
        """The ``edits`` sent to ``POST /v1/replays``."""
        if self._response is not _UNSET:
            return [{"step": self.at_step, "response": self._response}]
        if self._tool_result is not _UNSET:
            return [{"step": self.at_step, "tool_result": self._tool_result}]
        return []

    def live_after(self) -> Optional[int]:
        """The ``live_after`` sent to ``POST /v1/replays``: the edited step,
        or the step before the cut; ``None`` when not live."""
        if not self.live:
            return None
        return self.at_step if self.edits() else self.at_step - 1

    # ---------------------------------------------------------- lifecycle

    def start(self) -> "Fork":
        """Creates the replay, starts the switch, points the environment at
        it and starts tagging spans. ``__enter__`` calls it.

        Refused with ``RuntimeError``, before anything is created, when this
        fork would set the environment while another fork's block owns it,
        or tag every thread while another fork does."""
        global _ENV_OWNER, _THREAD_TAGGER
        if self.replay is not None:
            raise RuntimeError("this fork has already started")
        with _LOCK:
            if self.set_env and _ENV_OWNER is not None:
                raise RuntimeError(
                    f"fork {_ENV_OWNER.id} already points this process's OPENAI_BASE_URL and "
                    "ANTHROPIC_BASE_URL at its switch; forks that overlap take set_env=False and give "
                    "each agent fork.openai_kwargs() or fork.env()"
                )
            if self.tag_threads and _THREAD_TAGGER is not None:
                raise RuntimeError(
                    f"fork {_THREAD_TAGGER.id} already tags every thread's spans; at most one fork may "
                    "use tag_threads=True at a time"
                )
            # Claimed before the replay is created, so two forks starting
            # together cannot both pass the check.
            if self.set_env:
                _ENV_OWNER = self
            if self.tag_threads:
                _THREAD_TAGGER = self
        try:
            self.replay = self.client.replay(
                self.parent, mode=self.mode, edits=self.edits(), live_after=self.live_after()
            )
            self.id = self.replay.fork_id
            self.step = self.replay.fork.step if self.replay.fork is not None else None
            if self.live:
                self._switch = _Switch(self)
                with _LOCK:
                    _SWITCHES[self._switch.url] = self
            if self.set_env:
                for key, value in self.env().items():
                    self._saved_env[key] = os.environ.get(key)
                    os.environ[key] = value
        except BaseException:
            self.stop()
            raise
        self._token = _ACTIVE.set(self)
        if self.tag_spans:
            self.tagging = install_span_tags()
        return self

    def stop(self) -> None:
        """Undoes :meth:`start`: stops tagging, restores the environment,
        stops the switch, and keeps the replay's last status in
        :attr:`final_status`. ``__exit__`` calls it."""
        global _ENV_OWNER, _THREAD_TAGGER
        if self._token is not None:
            try:
                _ACTIVE.reset(self._token)
            except ValueError:
                # Exited in another context than it was entered in.
                _ACTIVE.set(None)
            self._token = None
        with _LOCK:
            if _THREAD_TAGGER is self:
                _THREAD_TAGGER = None
            if _ENV_OWNER is self:
                for key, value in self._saved_env.items():
                    if value is None:
                        os.environ.pop(key, None)
                    else:
                        os.environ[key] = value
                _ENV_OWNER = None
            self._saved_env = {}
            if self._switch is not None:
                _SWITCHES.pop(self._switch.url, None)
        if self._switch is not None:
            self._switch.close()
            self._switch = None
        if self.replay is not None:
            try:
                self.final_status = self.replay.status()
            except TrazaError:
                pass

    def __enter__(self) -> "Fork":
        return self.start()

    def __exit__(self, *exc_info: Any) -> None:
        self.stop()

    # ---------------------------------------------------------- what the agent uses

    @property
    def run_id(self) -> str:
        """The session id the forked run records under: ``run_id`` as given,
        else the fork id."""
        return self._run_id or self._require_id()

    @property
    def is_live(self) -> bool:
        """Whether model calls now go to the real provider: the replay has
        answered one ``replay_live``. Only the model endpoint's answer says
        so — a tool the recording lacks going live leaves the recorded model
        calls on the recording."""
        return self.live_at is not None

    @property
    def attributes(self) -> Dict[str, Any]:
        """The fork's tags, as the server returned them."""
        if self.replay is not None and self.replay.fork is not None and self.replay.fork.attributes:
            return dict(self.replay.fork.attributes)
        return {FORK_PARENT: self.parent, FORK_STEP: self.step, FORK_ID: self._require_id()}

    def span_attributes(self) -> Dict[str, Any]:
        """What :class:`ForkSpanProcessor` stamps: the tags, and the fork's
        session id unless ``tag_session=False``."""
        attributes = self.attributes
        if self.tag_session:
            attributes["session.id"] = self.run_id
        return {key: value for key, value in attributes.items() if value is not None}

    @property
    def openai_base_url(self) -> str:
        """The base URL an OpenAI SDK uses inside the fork."""
        if self._switch is not None:
            return self._switch.url + "/openai/v1"
        return self._require_replay().openai_base_url

    @property
    def anthropic_base_url(self) -> str:
        """The base URL an Anthropic SDK uses inside the fork."""
        if self._switch is not None:
            return self._switch.url + "/anthropic"
        return self._require_replay().anthropic_base_url

    @property
    def api_key(self) -> str:
        """The key the agent's SDK sends: the switch's per-fork secret, or —
        without a switch — the client's Traza token."""
        if self._switch is not None:
            return self._switch.secret
        return self.client.token or "traza-open"

    def openai_kwargs(self) -> Dict[str, str]:
        """``OpenAI(**fork.openai_kwargs())``."""
        return {"base_url": self.openai_base_url, "api_key": self.api_key}

    def anthropic_kwargs(self) -> Dict[str, str]:
        """``Anthropic(**fork.anthropic_kwargs())``."""
        return {"base_url": self.anthropic_base_url, "api_key": self.api_key}

    def env(self) -> Dict[str, str]:
        """The environment that points an agent — this process, or a
        subprocess — at the fork, with the fork's identity for an agent that
        tags its own spans: ``TRAZA_FORK_ID``, ``TRAZA_FORK_PARENT``,
        ``TRAZA_FORK_STEP``, ``TRAZA_FORK_RUN`` (its session id) and
        ``TRAZA_REPLAY_ID``."""
        replay = self._require_replay()
        return {
            "OPENAI_BASE_URL": self.openai_base_url,
            "OPENAI_API_KEY": self.api_key,
            "ANTHROPIC_BASE_URL": self.anthropic_base_url,
            "ANTHROPIC_API_KEY": self.api_key,
            "TRAZA_FORK_ID": self._require_id(),
            "TRAZA_FORK_PARENT": self.parent,
            "TRAZA_FORK_STEP": str(self.step if self.step is not None else self.at_step),
            "TRAZA_FORK_RUN": self.run_id,
            "TRAZA_REPLAY_ID": replay.id or "",
        }

    def tool_result(self, name: Optional[str] = None) -> Optional[ToolStep]:
        """The next recorded tool step (of ``name``), with an edited result
        where the fork has one — or ``None`` when the harness should run the
        real tool: past the cut, or with no recorded step of that tool left.

        ``None`` says nothing about model calls. The server's tool cursor is
        independent of its model cursor, so a tool the recording does not
        hold answers this way while recorded model calls are still to come,
        and the switch keeps serving those from the recording until the
        model endpoint itself answers ``replay_live``."""
        try:
            step = self._require_replay().next_tool(name)
        except TrazaError as error:
            if error.status == 409 and error.type == "replay_live":
                return None
            raise
        return step

    def call_tool(self, name: str, tool: Callable[..., Any], *args: Any, **kwargs: Any) -> Any:
        """Runs ``tool`` — unless the fork still has a recorded (or edited)
        result for it, which is returned instead."""
        step = self.tool_result(name)
        if step is not None:
            return step.result
        return tool(*args, **kwargs)

    def status(self) -> ReplayStatus:
        """The replay's status: what was served, the edits, the live point."""
        return self._require_replay().status()

    def diff(self) -> Diff:
        """The parent run against the forked run, once the forked run's spans
        are recorded under :attr:`run_id`."""
        return self.client.diff(self.parent, self.run_id)

    # ---------------------------------------------------------- internals

    def _models_went_live(self) -> None:
        """Notes the model endpoint's ``replay_live``: from here every model
        call goes to the real provider. Called by the switch alone."""
        if self.live_at is None:
            self.live_at = time.time()

    def _require_replay(self) -> Replay:
        if self.replay is None:
            raise RuntimeError("this fork has not started; use it as a context manager or call start()")
        return self.replay

    def _require_id(self) -> str:
        if not self.id:
            raise RuntimeError("this fork has not started, or the server returned no fork id")
        return self.id

    def __repr__(self) -> str:
        return f"Fork(parent={self.parent!r}, at_step={self.at_step}, id={self.id!r}, live={self.is_live})"


class _SwitchServer(ThreadingHTTPServer):
    daemon_threads = True

    def server_bind(self) -> None:
        # HTTPServer.server_bind asks for the address's fully qualified name,
        # a reverse lookup that can stall a fork's start on a slow resolver;
        # nothing here uses it.
        socketserver.TCPServer.server_bind(self)
        self.server_name, self.server_port = self.server_address[:2]


class _Switch:
    """The loopback server that forwards a fork's model calls: to the replay
    until it answers ``replay_live``, to the real provider after."""

    def __init__(self, fork: Fork) -> None:
        self.fork = fork
        self.secret = _SECRET_PREFIX + secrets.token_urlsafe(32)
        server = _SwitchServer(("127.0.0.1", 0), _SwitchHandler)
        server.switch = self  # type: ignore[attr-defined]
        self._server = server
        self.url = f"http://127.0.0.1:{server.server_address[1]}"
        self._thread = threading.Thread(target=server.serve_forever, name="traza-fork-switch", daemon=True)
        self._thread.start()

    def close(self) -> None:
        self._server.shutdown()
        self._server.server_close()
        self._thread.join(timeout=5)

    def authorized(self, headers: Mapping[str, str]) -> bool:
        presented = headers.get("x-api-key") or ""
        authorization = headers.get("authorization") or ""
        if authorization.lower().startswith("bearer "):
            presented = authorization[7:].strip()
        return bool(presented) and hmac.compare_digest(presented.encode(), self.secret.encode())

    def targets(self, path: str) -> Optional[Tuple[str, str, str]]:
        """``(provider, replay URL, live URL)`` for a request path."""
        replay = self.fork._require_replay()
        for provider, prefix, replay_base, live_base in (
            ("openai", "/openai/v1", replay.openai_base_url, self.fork.live_openai_base_url),
            ("anthropic", "/anthropic", replay.anthropic_base_url, self.fork.live_anthropic_base_url),
        ):
            if path == prefix or path.startswith(prefix + "/") or path.startswith(prefix + "?"):
                rest = path[len(prefix):]
                return provider, replay_base + rest, live_base + rest
        return None


class _SwitchHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server_version = "traza-fork-switch"

    def log_message(self, format: str, *args: Any) -> None:
        return None

    def do_POST(self) -> None:
        self._relay()

    do_GET = do_POST
    do_PUT = do_POST
    do_PATCH = do_POST
    do_DELETE = do_POST

    def _relay(self) -> None:
        switch: _Switch = self.server.switch  # type: ignore[attr-defined]
        self.close_connection = True
        headers = {name.lower(): value for name, value in self.headers.items()}
        if not switch.authorized(headers):
            self._json(401, {"error": {"type": "authentication_error",
                                       "message": "this fork's switch answers only the key the fork set"}})
            return
        targets = switch.targets(self.path)
        if targets is None:
            self._json(404, {"error": {"type": "not_found_error",
                                       "message": f"the fork switch serves /openai/v1/… and /anthropic/…, not {self.path}"}})
            return
        provider, replay_url, live_url = targets
        length = int(headers.get("content-length") or 0)
        body = self.rfile.read(length) if length > 0 else b""
        forwarded = {name: value for name, value in self.headers.items() if name.lower() not in _HOP_BY_HOP}
        fork = switch.fork
        if not fork.is_live:
            token = fork.client.token or "traza-open"
            try:
                status, reason, response_headers, content = _exchange(
                    self.command, replay_url, dict(forwarded, Authorization="Bearer " + token), body,
                    fork.client.timeout,
                )
            except OSError as error:
                self._json(502, {"error": {"type": "fork_switch_error",
                                           "message": f"the replay could not be reached: {error}"}})
                return
            if not (status == 409 and _is_live_signal(content)):
                self._write(status, reason, response_headers, content)
                return
            fork._models_went_live()
        live_headers = dict(forwarded)
        if provider == "openai" and fork.live_openai_api_key:
            live_headers["Authorization"] = "Bearer " + fork.live_openai_api_key
        if provider == "anthropic" and fork.live_anthropic_api_key:
            live_headers["x-api-key"] = fork.live_anthropic_api_key
        try:
            self._stream(self.command, live_url, live_headers, body)
        except OSError as error:
            self._json(502, {"error": {"type": "fork_switch_error",
                                       "message": f"the live provider could not be reached: {error}"}})

    def _json(self, status: int, body: Any) -> None:
        payload = json.dumps(body).encode("utf-8")
        self._write(status, None, [("Content-Type", "application/json")], payload)

    def _write(self, status: int, reason: Optional[str], headers: List[Tuple[str, str]], body: bytes) -> None:
        self.send_response(status, reason)
        for name, value in headers:
            if name.lower() not in _HOP_BY_HOP:
                self.send_header(name, value)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)

    def _stream(self, method: str, url: str, headers: Dict[str, str], body: bytes) -> None:
        connection = _connect(url, LIVE_TIMEOUT_S)
        try:
            target = _target(url)
            connection.request(method, target, body=body or None, headers=headers)
            response = connection.getresponse()
            self.send_response(response.status, response.reason)
            for name, value in response.getheaders():
                if name.lower() not in _HOP_BY_HOP:
                    self.send_header(name, value)
            self.send_header("Transfer-Encoding", "chunked")
            self.send_header("Connection", "close")
            self.end_headers()
            read = getattr(response, "read1", None) or response.read
            while True:
                chunk = read(65536)
                if not chunk:
                    break
                self.wfile.write(b"%x\r\n%s\r\n" % (len(chunk), chunk))
                self.wfile.flush()
            self.wfile.write(b"0\r\n\r\n")
        finally:
            connection.close()


def _is_live_signal(content: bytes) -> bool:
    try:
        error = json.loads(content.decode("utf-8")).get("error")
    except (ValueError, UnicodeDecodeError, AttributeError):
        return False
    return isinstance(error, dict) and error.get("type") == "replay_live"


def _connect(url: str, timeout: float) -> http.client.HTTPConnection:
    parts = urllib.parse.urlsplit(url)
    if parts.scheme == "https":
        return http.client.HTTPSConnection(
            parts.hostname or "", parts.port, timeout=timeout, context=ssl.create_default_context()
        )
    if parts.scheme == "http":
        return http.client.HTTPConnection(parts.hostname or "", parts.port, timeout=timeout)
    raise OSError(f"unsupported URL scheme in {url!r}")


def _target(url: str) -> str:
    parts = urllib.parse.urlsplit(url)
    return (parts.path or "/") + (("?" + parts.query) if parts.query else "")


def _exchange(
    method: str, url: str, headers: Dict[str, str], body: bytes, timeout: float
) -> Tuple[int, str, List[Tuple[str, str]], bytes]:
    connection = _connect(url, timeout)
    try:
        connection.request(method, _target(url), body=body or None, headers=headers)
        response = connection.getresponse()
        return response.status, response.reason, response.getheaders(), response.read()
    finally:
        connection.close()
