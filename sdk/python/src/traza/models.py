"""Typed views of Traza's responses.

Each dataclass mirrors one documented response shape field for field, with
the server's own names. Every top-level response also keeps the document it
was read from in ``raw``, so a field added by a newer server is reachable
before this client learns its name. Fields the server omits are ``None``.

Timestamps are integer Unix nanoseconds, as on the wire. Python integers are
exact at any size, so nothing here rounds them.
"""

from __future__ import annotations

import dataclasses
import hashlib
import json
import typing
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any, Dict, List, Mapping, Optional, Type, TypeVar, Union, cast

if TYPE_CHECKING:  # pragma: no cover - typing only
    from .client import Client

__all__ = [
    "CredentialRef",
    "Identity",
    "LifetimeCounters",
    "MonthCounters",
    "TenantLimits",
    "TenantUsage",
    "UsageReport",
    "SpanRef",
    "TopRepeat",
    "HandoffCycle",
    "RunUsage",
    "DeliveredNudge",
    "Limits",
    "Verdict",
    "PolicyMatch",
    "Policy",
    "GlobalPolicy",
    "Policies",
    "FuseEvent",
    "StepTokens",
    "Step",
    "Tape",
    "Replay",
    "ForkEdit",
    "ForkInfo",
    "ForkState",
    "Divergence",
    "ReplayCall",
    "ReplayStatus",
    "ToolStep",
    "RunSummary",
    "AlignedStep",
    "FirstDivergence",
    "DiffDelta",
    "Diff",
    "FailureCause",
    "RecalledRun",
    "Recall",
    "ForkSummary",
    "Forks",
    "GoldenRun",
    "Goldens",
    "LedgerHead",
    "EvidenceWindow",
    "DayActivity",
    "Activity",
    "EvidencePack",
]


def _raw() -> Any:
    return field(default_factory=dict, repr=False, compare=False)


# ---------------------------------------------------------------- identity


@dataclass
class CredentialRef:
    id: Optional[str] = None
    label: Optional[str] = None


@dataclass
class Identity:
    """``GET /v1/whoami``: what the calling credential is."""

    #: ``token``, or ``open`` when the server has no credential at all.
    auth: Optional[str] = None
    #: ``ro``, ``rw`` or ``admin`` (``admin`` in open mode).
    scope: Optional[str] = None
    #: The tenant the credential is bound to; ``None`` when unbound.
    tenant: Optional[str] = None
    #: ``True`` for an unbound credential, which may use operator surfaces.
    operator: Optional[bool] = None
    #: The credential that authenticated; ``None`` in open mode.
    credential: Optional[CredentialRef] = None
    raw: Dict[str, Any] = _raw()


# ---------------------------------------------------------------- usage


@dataclass
class LifetimeCounters:
    spans: Optional[int] = None
    bytes: Optional[int] = None
    batches: Optional[int] = None
    rejected: Optional[int] = None


@dataclass
class MonthCounters:
    #: The UTC calendar month, ``YYYY-MM``.
    window: Optional[str] = None
    spans: Optional[int] = None
    bytes: Optional[int] = None


@dataclass
class TenantLimits:
    monthly_spans: Optional[int] = None
    monthly_bytes: Optional[int] = None
    ingest_paused: Optional[bool] = None
    ttl_seconds: Optional[int] = None


@dataclass
class TenantUsage:
    #: The tenant; ``""`` is the default tenant.
    tenant: Optional[str] = None
    lifetime: Optional[LifetimeCounters] = None
    month: Optional[MonthCounters] = None
    limits: Optional[TenantLimits] = None
    #: ``ok``, ``warning`` (80% of a monthly limit), ``capped`` or ``paused``.
    state: Optional[str] = None


@dataclass
class UsageReport:
    """``GET /v1/usage``: what each tenant has sent, against its limits."""

    as_of_ns: Optional[int] = None
    tenants: List[TenantUsage] = field(default_factory=list)
    raw: Dict[str, Any] = _raw()


# ---------------------------------------------------------------- fuses


@dataclass
class SpanRef:
    trace_id: Optional[str] = None
    span_id: Optional[str] = None


@dataclass
class TopRepeat:
    #: ``kind:name#<16 hex of the input digest>``.
    fingerprint: Optional[str] = None
    #: Occurrences among the run's last ``window_steps`` steps.
    count: Optional[int] = None
    #: The steps counted over: the policy's ``repeat_window_steps``.
    window_steps: Optional[int] = None
    example: Optional[SpanRef] = None


@dataclass
class HandoffCycle:
    """Control returning to one named agent straight from another."""

    #: The agent control returned to, then the agent it returned from.
    agents: List[str] = field(default_factory=list)
    count: Optional[int] = None


@dataclass
class RunUsage:
    steps: Optional[int] = None
    llm_calls: Optional[int] = None
    tool_calls: Optional[int] = None
    tokens: Optional[int] = None
    cost_usd: Optional[float] = None
    errors: Optional[int] = None
    error_streak: Optional[int] = None
    duration_ns: Optional[int] = None
    top_repeat: Optional[TopRepeat] = None
    #: The most frequent handoff cycle since the last reset; ``None`` when
    #: control never returned to a named agent.
    top_handoff_cycle: Optional[HandoffCycle] = None


@dataclass
class DeliveredNudge:
    """A nudge a pre-call check delivered: ``advice`` for the agent to hear,
    or a ``block`` that vetoed the step it asked about."""

    id: Optional[str] = None
    #: ``advice`` or ``block``.
    kind: Optional[str] = None
    message: Optional[str] = None


@dataclass
class Limits:
    """A fuse policy's limits, or a reset's grant. ``None`` is unbounded."""

    max_cost_usd: Optional[float] = None
    max_tokens: Optional[int] = None
    max_steps: Optional[int] = None
    max_llm_calls: Optional[int] = None
    max_tool_calls: Optional[int] = None
    max_duration_seconds: Optional[int] = None
    max_repeats: Optional[int] = None
    #: The steps ``max_repeats`` counts over (the server's default is 50).
    repeat_window_steps: Optional[int] = None
    max_error_streak: Optional[int] = None
    max_handoff_cycles: Optional[int] = None

    def to_dict(self) -> Dict[str, Any]:
        """The limits that are set, as the wire spells them."""
        return {f.name: getattr(self, f.name) for f in dataclasses.fields(self) if getattr(self, f.name) is not None}


@dataclass
class Verdict:
    """The verdict document every run-fuse route answers."""

    run: Optional[str] = None
    tenant: Optional[str] = None
    #: ``continue``, ``warn`` or ``stop``.
    verdict: Optional[str] = None
    reasons: List[str] = field(default_factory=list)
    #: The policy the run is held to; ``global`` for the server default.
    policy: Optional[str] = None
    usage: Optional[RunUsage] = None
    limits: Optional[Limits] = None
    grant: Optional[Limits] = None
    tripped_at_ns: Optional[int] = None
    #: ``policy`` or ``manual`` for a recorded stop; ``None`` otherwise,
    #: including for a hypothetical stop from :meth:`Client.check`.
    tripped_by: Optional[str] = None
    evidence: List[SpanRef] = field(default_factory=list)
    #: ``live`` from memory, ``rebuilt`` when answering read stored spans.
    source: Optional[str] = None
    #: The nudges this answer delivered — :meth:`Client.check` only. Each is
    #: delivered once: the check that carries it consumes it, so read them
    #: here (or pass ``on_nudges`` to :meth:`Client.guard`).
    nudges: List[DeliveredNudge] = field(default_factory=list)
    raw: Dict[str, Any] = _raw()

    @property
    def stopped(self) -> bool:
        return self.verdict == "stop"

    @property
    def advice(self) -> List[str]:
        """The messages of the ``advice`` nudges this answer delivered."""
        return [nudge.message or "" for nudge in self.nudges if nudge.kind == "advice"]

    @property
    def warned(self) -> bool:
        return self.verdict == "warn"


@dataclass
class PolicyMatch:
    service: Optional[str] = None


@dataclass
class Policy:
    id: Optional[str] = None
    tenant: Optional[str] = None
    name: Optional[str] = None
    match: Optional[PolicyMatch] = None
    limits: Optional[Limits] = None
    #: ``stop`` or ``warn``.
    action: Optional[str] = None
    created_at_ns: Optional[int] = None
    raw: Dict[str, Any] = _raw()


@dataclass
class GlobalPolicy:
    """The server-wide default from the ``--fuse-*`` flags."""

    name: Optional[str] = None
    action: Optional[str] = None
    limits: Optional[Limits] = None


@dataclass
class Policies:
    """``GET /v1/fuses/policies``."""

    policies: List[Policy] = field(default_factory=list)
    #: The wire's ``global`` field (a Python keyword, hence the name).
    global_policy: Optional[GlobalPolicy] = field(default=None, metadata={"key": "global"})
    raw: Dict[str, Any] = _raw()


@dataclass
class FuseEvent:
    """One verdict transition from ``GET /v1/fuses/events``."""

    cursor: Optional[str] = None
    at_ns: Optional[int] = None
    tenant: Optional[str] = None
    run: Optional[str] = None
    #: ``warn``, ``stop``, ``manual_stop`` or ``reset``.
    kind: Optional[str] = None
    reasons: List[str] = field(default_factory=list)
    policy: Optional[str] = None
    raw: Dict[str, Any] = _raw()


# ---------------------------------------------------------------- tapes


@dataclass
class StepTokens:
    """Token counts a step reported; ``None`` when not reported, never 0."""

    input: Optional[int] = None
    output: Optional[int] = None
    cache_read: Optional[int] = None
    cache_write: Optional[int] = None
    #: The context the call carried: input plus the cache counters for a
    #: provider whose input leaves its cache out (Anthropic), input alone for
    #: one whose input includes it (OpenAI); ``None`` when it cannot be told.
    context: Optional[int] = None


@dataclass
class Step:
    index: Optional[int] = None
    #: ``llm``, ``tool`` or ``agent``.
    kind: Optional[str] = None
    name: Optional[str] = None
    service: Optional[str] = None
    status: Optional[str] = None
    trace_id: Optional[str] = None
    span_id: Optional[str] = None
    parent_span_id: Optional[str] = None
    start_ns: Optional[int] = None
    duration_ns: Optional[int] = None
    model: Optional[str] = None
    tool: Optional[str] = None
    agent: Optional[str] = None
    #: The step's input as stored; may be a ``{"$payload": ...}`` reference
    #: unless the tape was read with ``resolve=True``.
    request: Any = None
    response: Any = None
    request_digest: Optional[str] = None
    response_digest: Optional[str] = None
    fingerprint: Optional[str] = None
    tokens: Optional[StepTokens] = None
    cost_usd: Optional[float] = None
    raw: Dict[str, Any] = _raw()


@dataclass
class Tape:
    """``GET /v1/tapes/{run}``: a run's steps in order."""

    run: Optional[str] = None
    #: The tenant scope read under; ``None`` is the operator view.
    tenant: Optional[str] = None
    steps: List[Step] = field(default_factory=list)
    truncated: Optional[bool] = None
    spans_examined: Optional[int] = None
    #: Only with ``resolve=True``: some payload reference stayed a reference.
    resolved_partial: Optional[bool] = None
    raw: Dict[str, Any] = _raw()


# ---------------------------------------------------------------- replays


@dataclass
class Divergence:
    step: Optional[int] = None
    expected_digest: Optional[str] = None
    got_digest: Optional[str] = None
    #: Where the request first differed from the recording: ``message``,
    #: ``role``, ``field`` (``role``, ``content``, ``tool_call_id``,
    #: ``tool_calls``, or ``missing`` / ``extra``), ``at_char``, quotes of
    #: both sides as ``recorded`` and ``got``, and both message counts.
    #: ``None`` when it could not be said.
    difference: Optional[Dict[str, Any]] = None


@dataclass
class ForkEdit:
    step: Optional[int] = None
    #: ``response`` or ``tool_result``.
    kind: Optional[str] = None
    #: In a status document: whether the edit has been served.
    served: Optional[bool] = None


@dataclass
class ForkInfo:
    """A fork's ``fork`` object in a replay's creation response."""

    id: Optional[str] = None
    #: The run it branches from.
    parent: Optional[str] = None
    #: The first step whose answer is not the parent's.
    step: Optional[int] = None
    edits: List[ForkEdit] = field(default_factory=list)
    #: After this step every call answers ``replay_live``.
    live_after: Optional[int] = None
    #: The span attributes the forked run's spans carry.
    attributes: Dict[str, Any] = field(default_factory=dict)


@dataclass
class ForkState:
    """A fork's ``fork`` object in a replay's status document."""

    id: Optional[str] = None
    parent: Optional[str] = None
    step: Optional[int] = None
    edits: List[ForkEdit] = field(default_factory=list)
    live_after: Optional[int] = None
    #: When a call was first answered ``replay_live``.
    live_at_ns: Optional[int] = None
    live_calls: Optional[int] = None


@dataclass
class ReplayCall:
    step: Optional[int] = None
    matched: Optional[bool] = None
    at_ns: Optional[int] = None


@dataclass
class ReplayStatus:
    """``GET /v1/replays/{id}``: what a replay has served."""

    id: Optional[str] = None
    run: Optional[str] = None
    mode: Optional[str] = None
    served: Optional[int] = None
    remaining: Optional[int] = None
    diverged: Optional[Divergence] = None
    calls: List[ReplayCall] = field(default_factory=list)
    #: Model calls refused because every recorded step had been served.
    exhausted_calls: Optional[int] = None
    unreadable_tool_steps: List[int] = field(default_factory=list)
    expires_at_ns: Optional[int] = None
    #: Present when the replay is a fork.
    fork_id: Optional[str] = None
    fork: Optional[ForkState] = None
    raw: Dict[str, Any] = _raw()


@dataclass
class ToolStep:
    """A recorded tool step from ``POST /v1/replays/{id}/tools/next``."""

    step: Optional[int] = None
    tool: Optional[str] = None
    arguments: Any = None
    result: Any = None
    #: ``True`` when ``result`` is a fork's edit rather than the recording's.
    edited: Optional[bool] = None
    raw: Dict[str, Any] = _raw()


@dataclass
class Replay:
    """A replay created by ``POST /v1/replays``.

    Point an OpenAI or Anthropic SDK at :attr:`openai_base_url` or
    :attr:`anthropic_base_url` (or export :meth:`env`) and every model call
    the agent makes is answered from the recording. Tools still run live
    unless the harness takes their results from :meth:`next_tool`.
    """

    id: Optional[str] = None
    run: Optional[str] = None
    mode: Optional[str] = None
    steps: Optional[int] = None
    #: ``True`` when ``steps`` counts only the run's beginning: a fork of a
    #: run past the server's span cap that goes live within it.
    steps_truncated: bool = False
    llm_steps: Optional[int] = None
    expires_at_ns: Optional[int] = None
    unreadable_tool_steps: List[int] = field(default_factory=list)
    #: The server's base URLs, as paths: ``{"openai": ..., "anthropic": ...}``.
    base_urls: Dict[str, str] = field(default_factory=dict)
    #: Present when the replay is a fork (created with edits or a live point).
    fork_id: Optional[str] = None
    fork: Optional[ForkInfo] = None
    raw: Dict[str, Any] = _raw()
    # Typed Any rather than Client: decode() resolves every annotation with
    # get_type_hints, and Client is imported for type checkers only.
    _client: Any = field(default=None, repr=False, compare=False)

    @property
    def openai_base_url(self) -> str:
        """The absolute base URL for the OpenAI SDK (``…/openai/v1``)."""
        return self._absolute("openai")

    @property
    def anthropic_base_url(self) -> str:
        """The absolute base URL for the Anthropic SDK (``…/anthropic``)."""
        return self._absolute("anthropic")

    def env(self, api_key: Optional[str] = None) -> Dict[str, str]:
        """The environment that points both providers' SDKs at this replay.

        The API keys are ``api_key`` when given, else the client's Traza
        token: the OpenAI SDK sends it as a bearer token and the Anthropic
        SDK as ``x-api-key``, which Traza accepts on the replay routes. On an
        open server any non-empty key works, and the SDKs refuse to start
        without one, so a placeholder stands in.

        Whatever key this returns, every library in the process that gets
        the environment can read it, and one that reads ``OPENAI_API_KEY``
        but ignores ``OPENAI_BASE_URL`` sends it to the real provider. When
        the client holds an operator's token, pass a dedicated ``ro``
        credential bound to the run's tenant as ``api_key``: any valid
        credential of the replay's tenant may consume it.
        """
        key = api_key or self._require_client().token or "traza-open"
        return {
            "OPENAI_BASE_URL": self.openai_base_url,
            "OPENAI_API_KEY": key,
            "ANTHROPIC_BASE_URL": self.anthropic_base_url,
            "ANTHROPIC_API_KEY": key,
        }

    def status(self) -> ReplayStatus:
        """What the replay has served so far, and its first divergence."""
        return self._require_client()._replay_status(self._require_id())

    def next_tool(self, name: Optional[str] = None) -> Optional[ToolStep]:
        """The next recorded tool step not yet served (of ``name``, if given).

        Returns ``None`` when no unserved tool step remains, so a harness can
        loop until it runs out. A step whose recorded value cannot be read
        raises :class:`~traza.Conflict` (``type == "tool_unreadable"``) and
        is not consumed; a withdrawn replay raises :class:`~traza.Gone`.
        """
        return self._require_client()._replay_next_tool(self._require_id(), name)

    def _absolute(self, provider: str) -> str:
        path = self.base_urls.get(provider)
        if not path:
            raise ValueError(f"the server returned no {provider} base URL for replay {self.id}")
        # Concatenated rather than resolved with urljoin: the server's paths
        # are absolute (/v1/...), and urljoin would discard a path prefix the
        # client's URL carries when Traza sits behind a reverse proxy.
        return self._require_client().url + path

    def _require_client(self) -> "Client":
        if self._client is None:
            raise RuntimeError("this Replay was not created by a Client")
        return cast("Client", self._client)

    def _require_id(self) -> str:
        if not self.id:
            raise RuntimeError("this Replay has no id")
        return self.id


# ---------------------------------------------------------------- diff


@dataclass
class RunSummary:
    run: Optional[str] = None
    steps: Optional[int] = None
    cost_usd: Optional[float] = None
    tokens: Optional[int] = None
    duration_ns: Optional[int] = None
    errors: Optional[int] = None
    #: ``success``, ``failure``, ``abandoned`` or ``unknown``.
    outcome: Optional[str] = None
    truncated: Optional[bool] = None


@dataclass
class AlignedStep:
    #: ``same``, ``changed``, ``insert`` or ``delete``.
    op: Optional[str] = None
    a: Optional[int] = None
    b: Optional[int] = None
    fingerprint_a: Optional[str] = None
    fingerprint_b: Optional[str] = None
    #: ``input``, ``decision``, ``config`` or ``structure``; ``None`` if same.
    why: Optional[str] = None


@dataclass
class FirstDivergence:
    a: Optional[int] = None
    b: Optional[int] = None
    kind: Optional[str] = None
    explanation: Optional[str] = None


@dataclass
class DiffDelta:
    """``b`` minus ``a``."""

    cost_usd: Optional[float] = None
    tokens: Optional[int] = None
    duration_ns: Optional[int] = None
    errors: Optional[int] = None
    steps: Optional[int] = None


@dataclass
class Diff:
    """``GET /v1/diff``: two runs aligned, and where they first diverge."""

    a: Optional[RunSummary] = None
    b: Optional[RunSummary] = None
    aligned: List[AlignedStep] = field(default_factory=list)
    first_divergence: Optional[FirstDivergence] = None
    delta: Optional[DiffDelta] = None
    truncated: Optional[bool] = None
    raw: Dict[str, Any] = _raw()


# ---------------------------------------------------------------- recall


@dataclass
class FailureCause:
    trace_id: Optional[str] = None
    span_id: Optional[str] = None
    name: Optional[str] = None
    rule: Optional[str] = None


@dataclass
class RecalledRun:
    run: Optional[str] = None
    tenant: Optional[str] = None
    outcome: Optional[str] = None
    #: ``declared`` or ``derived``.
    outcome_source: Optional[str] = None
    goal: Optional[str] = None
    #: How many of the run's spans matched the query.
    score: Optional[int] = None
    last_end_ns: Optional[int] = None
    steps: Optional[int] = None
    cost_usd: Optional[float] = None
    #: Step signatures in order, consecutive repeats folded (``tool:x x3``).
    skeleton: List[str] = field(default_factory=list)
    skeleton_truncated: Optional[bool] = None
    truncated: Optional[bool] = None
    cause: Optional[FailureCause] = None


@dataclass
class Recall:
    """``GET /v1/recall``: earlier runs with the same words, successes first."""

    runs: List[RecalledRun] = field(default_factory=list)
    truncated: Optional[bool] = None
    raw: Dict[str, Any] = _raw()

    def __iter__(self) -> typing.Iterator[RecalledRun]:
        return iter(self.runs)

    def __len__(self) -> int:
        return len(self.runs)


# ---------------------------------------------------------------- forks


@dataclass
class ForkSummary:
    """One fork in ``GET /v1/forks``."""

    run: Optional[str] = None
    tenant: Optional[str] = None
    fork_id: Optional[str] = None
    fork_step: Optional[int] = None
    first_start_ns: Optional[int] = None
    last_end_ns: Optional[int] = None
    tagged_spans: Optional[int] = None
    #: Whether it was diffed against its parent; when not, ``skipped`` says
    #: why (``deadline`` or ``parent_missing``).
    diffed: Optional[bool] = None
    skipped: Optional[str] = None
    steps: Optional[int] = None
    outcome: Optional[str] = None
    cost_usd: Optional[float] = None
    errors: Optional[int] = None
    #: Where it parted from its parent (``a`` the parent, ``b`` the fork).
    first_divergence: Optional[FirstDivergence] = None
    delta: Optional[DiffDelta] = None
    diverged_before_cut: Optional[bool] = None
    truncated: Optional[bool] = None


@dataclass
class Forks:
    """``GET /v1/forks``: a run's forks, each diffed against it."""

    run: Optional[str] = None
    parent: Optional[RunSummary] = None
    forks: List[ForkSummary] = field(default_factory=list)
    #: Tagged spans that joined the parent run itself (a fork without a
    #: session of its own).
    merged_into_parent: Optional[int] = None
    spans_examined: Optional[int] = None
    truncated: Optional[bool] = None
    raw: Dict[str, Any] = _raw()

    def __iter__(self) -> typing.Iterator[ForkSummary]:
        return iter(self.forks)

    def __len__(self) -> int:
        return len(self.forks)


@dataclass
class GoldenRun:
    run: Optional[str] = None
    tenant: Optional[str] = None
    #: ``attribute`` or ``annotation``.
    source: Optional[str] = None
    tagged_at_ns: Optional[int] = None


@dataclass
class Goldens:
    """``GET /v1/goldens``: the runs marked as regression baselines."""

    runs: List[GoldenRun] = field(default_factory=list)
    spans_examined: Optional[int] = None
    annotations_examined: Optional[int] = None
    truncated: Optional[bool] = None
    raw: Dict[str, Any] = _raw()

    def __iter__(self) -> typing.Iterator[GoldenRun]:
        return iter(self.runs)

    def __len__(self) -> int:
        return len(self.runs)


# ---------------------------------------------------------------- ledger


@dataclass
class LedgerHead:
    """``GET /v1/ledger/head``: the chain's tip."""

    seq: Optional[int] = None
    head: Optional[str] = None
    store_id: Optional[str] = None
    at_ns: Optional[int] = None
    raw: Dict[str, Any] = _raw()


@dataclass
class EvidenceWindow:
    since_ns: Optional[int] = None
    until_ns: Optional[int] = None


@dataclass
class DayActivity:
    day: Optional[str] = None
    spans: Optional[int] = None
    errors: Optional[int] = None
    cost_usd: Optional[float] = None


@dataclass
class Activity:
    spans: Optional[int] = None
    traces: Optional[int] = None
    sessions: Optional[int] = None
    errors: Optional[int] = None
    by_day: List[DayActivity] = field(default_factory=list)


@dataclass
class EvidencePack:
    """``GET /v1/evidence``: the document a notary signs.

    ``ledger``, ``retention`` and ``erasures`` are kept as the server's JSON
    (see the ledger guide for their fields). :meth:`verify_digest` recomputes
    ``digest`` from the bytes the server sent.
    """

    format: Optional[str] = None
    digest: Optional[str] = None
    engine_version: Optional[str] = None
    generated_at_ns: Optional[int] = None
    store_id: Optional[str] = None
    tenant: Optional[str] = None
    window: Optional[EvidenceWindow] = None
    ledger: Dict[str, Any] = field(default_factory=dict)
    retention: Dict[str, Any] = field(default_factory=dict)
    usage: Optional[TenantUsage] = None
    activity: Optional[Activity] = None
    erasures: List[Dict[str, Any]] = field(default_factory=list)
    raw: Dict[str, Any] = _raw()
    _text: str = field(default="", repr=False, compare=False)

    def verify_digest(self) -> bool:
        """Recomputes ``digest`` over the pack as received.

        The digest is the SHA-256 of the canonical JSON (sorted keys, no
        whitespace) of every other field. Non-integer numbers are kept
        exactly as the server printed them: re-printing ``cost_usd`` in
        Python's float format would change the bytes and the digest with
        them. This is the check the ledger guide gives, run on the response
        body itself.
        """
        if not self._text:
            raise RuntimeError("this EvidencePack was not read from a response")
        pack = json.loads(self._text, parse_float=_RawNumber)
        claimed = pack.pop("digest", None)
        if not isinstance(claimed, str):
            return False
        return hashlib.sha256(_canonical(pack).encode("utf-8")).hexdigest() == claimed


class _RawNumber(str):
    """A non-integer JSON number, kept as the text the server wrote."""


def _canonical(value: Any) -> str:
    if isinstance(value, dict):
        return "{" + ",".join(json.dumps(key, ensure_ascii=False) + ":" + _canonical(value[key]) for key in sorted(value)) + "}"
    if isinstance(value, list):
        return "[" + ",".join(_canonical(item) for item in value) + "]"
    if isinstance(value, _RawNumber):
        return str(value)
    return json.dumps(value, ensure_ascii=False)


# ---------------------------------------------------------------- decoding

_HINTS: Dict[type, Dict[str, Any]] = {}

T = TypeVar("T")


def decode(cls: Type[T], data: Any) -> T:
    """Builds ``cls`` from a JSON object, recursively, by its type hints.

    Unknown keys are ignored here (and kept in ``raw`` where the class has
    one); absent keys keep the field's default.
    """
    if not isinstance(data, Mapping):
        raise TypeError(f"expected a JSON object for {cls.__name__}, got {type(data).__name__}")
    hints = _HINTS.get(cls)
    if hints is None:
        hints = typing.get_type_hints(cls)
        _HINTS[cls] = hints
    kwargs: Dict[str, Any] = {}
    for f in dataclasses.fields(cls):  # type: ignore[arg-type]  # every T here is a dataclass
        if f.name.startswith("_"):
            continue
        if f.name == "raw":
            kwargs["raw"] = dict(data)
            continue
        key = f.metadata.get("key", f.name)
        if key in data:
            kwargs[f.name] = _convert(hints[f.name], data[key])
    return cls(**kwargs)


def _convert(hint: Any, value: Any) -> Any:
    if value is None:
        return None
    origin = typing.get_origin(hint)
    args = typing.get_args(hint)
    if origin is Union:
        inner = [arg for arg in args if arg is not type(None)]
        return _convert(inner[0], value) if len(inner) == 1 else value
    if origin in (list, List):
        if not isinstance(value, list):
            return value
        return [_convert(args[0], item) for item in value] if args else list(value)
    if origin in (dict, Dict):
        if not isinstance(value, Mapping):
            return value
        return {k: _convert(args[1], v) for k, v in value.items()} if args else dict(value)
    if isinstance(hint, type) and dataclasses.is_dataclass(hint) and isinstance(value, Mapping):
        return decode(hint, value)
    if hint is float and isinstance(value, int) and not isinstance(value, bool):
        return float(value)
    return value
