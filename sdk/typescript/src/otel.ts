/**
 * The OpenTelemetry exporter environment for sending traces to Traza.
 *
 * Traza does not instrument anything: OpenTelemetry (or OpenLLMetry,
 * OpenInference) does, and its OTLP/HTTP exporter is configured by
 * environment variables. This only writes those variables down correctly.
 *
 * @module
 */

/** Options for {@link otelEnv}. */
export interface OtelEnvOptions {
  /** Sent as `Authorization: Bearer <token>`. */
  token?: string;
  /** Set as the `traza.tenant` resource attribute. */
  tenant?: string;
}

/**
 * The `OTEL_*` variables that point an OTLP exporter at Traza at `url`.
 *
 * They are the trace-signal variables (`OTEL_EXPORTER_OTLP_TRACES_*`), not
 * the generic `OTEL_EXPORTER_OTLP_*`: a generic endpoint would send metrics
 * and logs to Traza too, which ingests traces only, and an SDK with those
 * exporters on would log a failed export every interval.
 *
 * - the endpoint is `${url}/v1/traces` (a signal-specific endpoint is used as
 *   given, so the path is spelled out);
 * - the protocol is `http/protobuf`;
 * - with `token`, the headers carry `Authorization: Bearer <token>`,
 *   percent-encoded as the OpenTelemetry specification requires;
 * - with `tenant`, `OTEL_RESOURCE_ATTRIBUTES` sets `traza.tenant`, which
 *   replaces any value already exported — merge by hand if you set others.
 */
export function otelEnv(url: string, options: OtelEnvOptions = {}): Record<string, string> {
  const base = url.replace(/\/+$/, "");
  const env: Record<string, string> = {
    OTEL_EXPORTER_OTLP_TRACES_ENDPOINT: `${base}/v1/traces`,
    OTEL_EXPORTER_OTLP_TRACES_PROTOCOL: "http/protobuf",
  };
  if (options.token) {
    env.OTEL_EXPORTER_OTLP_TRACES_HEADERS = `Authorization=${encodeURIComponent(`Bearer ${options.token}`)}`;
  }
  if (options.tenant) {
    env.OTEL_RESOURCE_ATTRIBUTES = `traza.tenant=${encodeURIComponent(options.tenant)}`;
  }
  return env;
}
