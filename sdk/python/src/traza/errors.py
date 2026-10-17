"""Exceptions raised by the client.

Every refusal the server answers with an HTTP status becomes a
:class:`TrazaError`, or the subclass for its status, carrying the status, the
server's message, its machine-readable ``type`` when it sent one, and the
parsed body. :class:`FuseTripped` is deliberately *not* among them: a ``stop``
verdict is the answer to the question the guard asked, not a failed request,
and code that catches ``TrazaError`` to ride out an unreachable server must
not swallow it by accident.
"""

from __future__ import annotations

import json
from typing import TYPE_CHECKING, Any, Dict, List, Mapping, Optional

if TYPE_CHECKING:  # pragma: no cover - typing only
    from .models import Verdict

__all__ = [
    "TrazaError",
    "AuthError",
    "NotFound",
    "Conflict",
    "Gone",
    "QuotaExceeded",
    "FuseTripped",
]


class TrazaError(Exception):
    """A request the server refused, one that never reached it, or one whose
    answer could not be read.

    ``status`` is the HTTP status, or ``None`` when no response arrived
    (connection refused, DNS failure, timeout) or its status line could not
    be read. A redirect is one of these too, with its 3xx status: the client
    follows none. ``type`` is the machine-readable kind some routes send
    beside the message, such as ``replay_divergence`` or
    ``replay_exhausted``. ``body`` is the parsed JSON body, or the raw text
    when it was not JSON (an unreadable answer such as a proxy's HTML page
    carries the text that arrived).
    """

    def __init__(
        self,
        message: str,
        *,
        status: Optional[int] = None,
        type: Optional[str] = None,
        body: Any = None,
        headers: Optional[Mapping[str, str]] = None,
    ) -> None:
        super().__init__(message)
        self.message = message
        self.status = status
        self.type = type
        self.body = body
        self.headers = dict(headers or {})

    @property
    def retryable(self) -> bool:
        """``False`` when the server sent ``x-should-retry: false``.

        Traza sends it on refusals the same request can never get past — a
        strict replay's divergence, an exhausted or withdrawn replay, a run
        with no recorded answer — so a retry loop can stop at once.
        """
        return self.headers.get("x-should-retry", "").lower() != "false"

    def __str__(self) -> str:
        if self.status is None:
            return self.message
        return f"{self.status}: {self.message}"


class AuthError(TrazaError):
    """``401`` (no, unknown, expired or revoked token) or ``403``.

    For a ``403`` from the auth gate, ``required`` names the capability the
    credential lacked: ``rw``, ``admin`` or ``unbound``.
    """

    @property
    def required(self) -> Optional[str]:
        if isinstance(self.body, dict):
            value = self.body.get("required")
            return value if isinstance(value, str) else None
        return None


class NotFound(TrazaError):
    """``404``: no such run, replay, policy or route.

    Another tenant's object is a ``404`` too, never a ``403``: its existence
    is a fact about another tenant.
    """


class Conflict(TrazaError):
    """``409``: a conflict, such as a run that cannot be replayed, a strict
    replay's divergence, or a ``/v1/fuses`` route on a server started with
    ``--no-fuses``. Check :attr:`TrazaError.retryable` before retrying."""


class Gone(TrazaError):
    """``410``: a replay that an erasure withdrew, or one with nothing left to
    serve."""


class QuotaExceeded(TrazaError):
    """``429``: a tenant quota or ingest pause, or no room for another replay.

    ``retry_after`` is the server's ``Retry-After`` in seconds. ``quota`` is
    the refusal's ``quota`` object (tenant, dimension, limit, used, window,
    resets_at) for a quota or pause, and ``None`` for a replay refused for
    want of room.
    """

    @property
    def retry_after(self) -> Optional[float]:
        value = self.headers.get("retry-after")
        if value is None:
            return None
        try:
            return float(value)
        except ValueError:
            # RFC 9110 also allows an HTTP-date; Traza sends seconds, so an
            # unparseable value is reported as absent rather than guessed.
            return None

    @property
    def quota(self) -> Optional[Dict[str, Any]]:
        if isinstance(self.body, dict):
            quota = self.body.get("quota")
            if isinstance(quota, dict):
                return quota
        return None


class FuseTripped(Exception):
    """The run's fuse answered ``stop``: the next step must not run.

    Raised by :meth:`traza.Client.guard` before the guarded body executes.
    ``verdict`` is the full verdict document — reasons, usage, limits and the
    policy that stopped it.
    """

    def __init__(self, verdict: "Verdict") -> None:
        self.verdict = verdict
        reasons = "; ".join(verdict.reasons) or "stopped"
        super().__init__(f"run {verdict.run!r} is stopped: {reasons}")

    @property
    def reasons(self) -> List[str]:
        return list(self.verdict.reasons)


_BY_STATUS = {
    401: AuthError,
    403: AuthError,
    404: NotFound,
    409: Conflict,
    410: Gone,
    429: QuotaExceeded,
}


def error_from_response(status: int, reason: str, headers: Mapping[str, str], raw: bytes) -> TrazaError:
    """Builds the exception for a non-2xx response.

    Traza's own routes answer ``{"error": "<message>"}`` (with ``type`` beside
    it on the replay routes); the provider-shaped replay routes answer
    ``{"error": {"type", "message", ...}}``, wrapped in ``{"type": "error"}``
    on the Anthropic route. All three are read here so that every refusal
    surfaces with a message and, where the server gave one, a type.
    """
    text = raw.decode("utf-8", "replace")
    body: Any = text
    message = text.strip() or reason or f"HTTP {status}"
    kind: Optional[str] = None
    try:
        body = json.loads(text) if text else None
    except ValueError:
        pass
    if isinstance(body, dict):
        error = body.get("error")
        if isinstance(error, str):
            message = error
            kind = body.get("type") if isinstance(body.get("type"), str) else None
        elif isinstance(error, dict):
            message = str(error.get("message") or message)
            kind = error.get("type") if isinstance(error.get("type"), str) else None
    lowered = {name.lower(): value for name, value in headers.items()}
    cls = _BY_STATUS.get(status, TrazaError)
    return cls(message, status=status, type=kind, body=body, headers=lowered)
