# Leg 2: OTLP/HTTP JSON ingest

## Scope

`POST /v1/traces` accepting an OpenTelemetry OTLP/HTTP **JSON**
ExportTraceServiceRequest and mapping it onto the span model. No protobuf,
no new dependencies; the existing `/v1/spans` contract is untouched.

## Mapping

- resourceSpans[] x scopeSpans[] x spans[] flatten to individual spans.
- `traceId`/`spanId`/`parentSpanId`: OTLP JSON hex strings, stored as-is
  (lowercased); empty parent means root.
- `startTimeUnixNano`/`endTimeUnixNano`: OTLP JSON encodes u64 as STRING —
  accept both string and number.
- `name` -> name; `status.code` (STATUS_CODE_OK/ERROR/UNSET) -> status
  ("ok"/"error"/"").
- Resource attribute `service.name` -> service (fallback "unknown_service").
- Attributes: OTLP keyed AnyValue ({"stringValue":..}|{"intValue":..}|
  {"doubleValue":..}|{"boolValue":..}|{"arrayValue":..}) -> plain JSON
  values in span.attributes. Scope attributes merge under their own keys;
  span attributes win on collision.
- events[] -> span.events (name, timeUnixNano, attributes).
- Response: 200 {"partialSuccess":{}} on full accept; malformed JSON or
  structurally invalid request -> 400 with an error body.

## Acceptance (blocking, executable oracles)

1. `./ci.sh` green; every existing test unmodified and passing.
2. Conformance tests with embedded OTLP JSON fixtures (at minimum: a
   two-resource, two-scope request with typed attributes, string-encoded
   nanos, events, and a status ERROR span) asserting the exact mapped span
   fields via read-back through `/v1/traces/{id}`.
3. Spans ingested via OTLP are queryable through the existing filter API
   (service + attribute filters hit the indexes).
4. `/v1/spans`, `/v1/stats`, `/v1/flush` behavior unchanged (existing
   process-level tests are the oracle).

## Non-goals

Protobuf/gRPC OTLP, metrics/logs signals, gzip request bodies.
