"""A typed client for TrazaDB's agent-native APIs.

Not instrumentation — OpenTelemetry records the spans (see :func:`otel_env`
for its exporter settings). This package asks the server about what was
recorded: run fuses and the pre-call guard, tapes and replays, diffs, recall,
the ledger and evidence packs. Standard library only.

    from traza import Client

    traza = Client("http://127.0.0.1:8080", token="...")
    with traza.guard("sess-7f3a", est_cost_usd=0.02):
        ...  # runs only if the run's fuse allows another step
"""

# Defined before the submodules are imported: client.py reads it for its
# User-Agent.
__version__ = "0.27.0"

from .client import Client, EventStream, Guard  # noqa: E402
from .errors import (  # noqa: E402
    AuthError,
    Conflict,
    FuseTripped,
    Gone,
    NotFound,
    QuotaExceeded,
    TrazaError,
)
from .models import (  # noqa: E402
    Diff,
    EvidencePack,
    FuseEvent,
    Identity,
    LedgerHead,
    Limits,
    Policies,
    Policy,
    Recall,
    RecalledRun,
    Replay,
    ReplayStatus,
    Step,
    Tape,
    ToolStep,
    UsageReport,
    Verdict,
)
from .otel import otel_env  # noqa: E402

__all__ = [
    "__version__",
    "Client",
    "Guard",
    "EventStream",
    "otel_env",
    "TrazaError",
    "AuthError",
    "NotFound",
    "Conflict",
    "Gone",
    "QuotaExceeded",
    "FuseTripped",
    "Diff",
    "EvidencePack",
    "FuseEvent",
    "Identity",
    "LedgerHead",
    "Limits",
    "Policies",
    "Policy",
    "Recall",
    "RecalledRun",
    "Replay",
    "ReplayStatus",
    "Step",
    "Tape",
    "ToolStep",
    "UsageReport",
    "Verdict",
]
