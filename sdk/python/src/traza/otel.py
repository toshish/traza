"""The OpenTelemetry exporter environment for sending traces to Traza.

Traza does not instrument anything: OpenTelemetry (or OpenLLMetry,
OpenInference) does, and its OTLP/HTTP exporter is configured by environment
variables. This module only writes those variables down correctly.
"""

from __future__ import annotations

import urllib.parse
from typing import Dict, Optional

__all__ = ["otel_env"]


def otel_env(url: str, token: Optional[str] = None, tenant: Optional[str] = None) -> Dict[str, str]:
    """The ``OTEL_*`` variables that point an OTLP exporter at Traza.

    ``url`` is the server's base URL. The variables are the trace-signal
    ones (``OTEL_EXPORTER_OTLP_TRACES_*``) rather than the generic
    ``OTEL_EXPORTER_OTLP_*``: a generic endpoint would also send metrics and
    logs to Traza, which ingests traces only, so an SDK with those exporters
    on would log a failed export every interval.

    - the endpoint is ``{url}/v1/traces`` (a signal-specific endpoint is used
      as given, so the path is spelled out);
    - the protocol is ``http/protobuf``;
    - with ``token``, the headers carry ``Authorization: Bearer <token>``,
      percent-encoded as the OpenTelemetry specification requires (the space
      is ``%20``);
    - with ``tenant``, ``OTEL_RESOURCE_ATTRIBUTES`` sets ``traza.tenant``,
      the resource attribute Traza reads a span's tenant from. This replaces
      any ``OTEL_RESOURCE_ATTRIBUTES`` already exported; merge by hand if you
      set others, such as ``service.name``.
    """
    base = url.rstrip("/")
    env = {
        "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT": base + "/v1/traces",
        "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL": "http/protobuf",
    }
    if token:
        env["OTEL_EXPORTER_OTLP_TRACES_HEADERS"] = "Authorization=" + urllib.parse.quote("Bearer " + token, safe="")
    if tenant is not None and tenant != "":
        env["OTEL_RESOURCE_ATTRIBUTES"] = "traza.tenant=" + urllib.parse.quote(tenant, safe="")
    return env
