# Ingest

Traza accepts spans on two routes. They write to the same store, honour the
same `(trace_id, span_id)` primary key, and return under the same durability
contract. The only difference is the wire format.

| Route | Format | Use it when |
|---|---|---|
| `POST /v1/spans` | Traza's native JSON | You control the producer and want the smallest possible client |
| `POST /v1/traces` | OTLP/HTTP, protobuf or JSON | You are pointing an OpenTelemetry SDK, Collector, or any OTLP exporter at Traza |

Both are gated by the same authentication and both need the `rw` scope when
[authentication](../operations/administration.md#authentication) is configured.

## Which one should I use?

**Use `POST /v1/traces` if you already have OpenTelemetry instrumentation.**
That is the whole point of it: two environment variables and your existing SDK
exports to Traza with nothing else to write. You keep OTel's batching, retry,
and sampling.

**Use `POST /v1/spans` if you are emitting spans yourself.** The body is a
plain JSON array, so a producer is a few lines in any language and needs no
OTel dependency. It is also the route the `seed` tool, the benchmarks, and most
of the test suite use.

There is no functional reason to prefer one over the other once a span is
stored: an OTLP span and a native span with the same fields are the same
record.

## `POST /v1/spans` — native JSON

The body is either a bare array or an object with a `spans` key. The first
non-whitespace byte decides which, so there is no ambiguity and no fallback
parse.

```sh
curl -X POST http://localhost:8080/v1/spans \
  -H 'Content-Type: application/json' \
  -d '[{"trace_id":"trace-1","span_id":"span-1","name":"charge","service":"checkout",
        "start_time_unix_nano":1700000000000000000,
        "end_time_unix_nano":1700000000002500000,
        "status":"ok","attributes":{"region":"us-east"}}]'
```

```json
{"accepted":1,"durability":"wal"}
```

The equivalent envelope form:

```sh
curl -X POST http://localhost:8080/v1/spans \
  -H 'Content-Type: application/json' \
  -d '{"spans":[{"trace_id":"trace-1","span_id":"span-1","name":"charge","service":"checkout",
                 "start_time_ns":1700000000000000000,"end_time_ns":1700000000002500000}]}'
```

`accepted` counts the spans in the batch. `durability` echoes what the
acknowledgement guarantees — `buffered`, `wal`, or `flushed`. See
[durability](../operations/durability.md).

### Validation

A batch is atomic with respect to validation: if any span is invalid, nothing
from the batch is stored.

| Condition | Response |
|---|---|
| Body is neither an array nor `{"spans": …}` | `400 {"error":"body must be an array or {spans: [...]}"}` |
| A required field is missing | `400 {"error":"missing field \`name\` at line 1 column 31"}` |
| `trace_id` is empty | `400 {"error":"span 0: trace_id is empty"}` |
| `span_id` is empty | `400 {"error":"span 0: span_id is empty"}` |
| The store could not accept the write | `503` — retry with backoff |

The index in the error message is the span's position in the batch.

### Batching

Batching is the single largest lever on ingest throughput, because the
per-batch costs — the writer lock, the log frame, the fsync — are paid once per
request rather than once per span. The benchmarks use 1,000 spans per request.
Bodies are capped at 64 MiB.

Connections are persistent (HTTP/1.1 keep-alive) by default, so a client
sending many batches should reuse one connection rather than reconnecting per
request.

## `POST /v1/traces` — OTLP/HTTP

Point any OpenTelemetry SDK at Traza:

```sh
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:8080
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf   # or http/json
export OTEL_EXPORTER_OTLP_COMPRESSION=none
```

The route accepts an `ExportTraceServiceRequest`. `Content-Type:
application/x-protobuf` selects the binary decoder; anything else is parsed as
OTLP/HTTP JSON. The protobuf decoder lowers its input to exactly the JSON shape
the JSON decoder expects and then shares the same mapping — one mapping, two
encodings, so every conformance behaviour applies to both.

**gRPC is not served.** Use the `http/protobuf` exporter setting, which every
OTel SDK supports.

A JSON request:

```sh
curl -X POST http://localhost:8080/v1/traces \
  -H 'Content-Type: application/json' \
  -d '{"resourceSpans":[{
        "resource":{"attributes":[{"key":"service.name","value":{"stringValue":"otlp-demo"}}]},
        "scopeSpans":[{"spans":[{
          "traceId":"5b8efff798038103d269b633813fc60c",
          "spanId":"eee19b7ec3c1b174",
          "name":"GET /orders",
          "startTimeUnixNano":"1700000002000000000",
          "endTimeUnixNano":"1700000002050000000",
          "attributes":[{"key":"http.status_code","value":{"intValue":"200"}}]
        }]}]}]}'
```

```json
{"partialSuccess":{}}
```

Reading it back shows the mapping:

```sh
curl http://localhost:8080/v1/traces/5b8efff798038103d269b633813fc60c
```

```json
{"annotations":[],"spans":[{"attributes":{"http.status_code":200},"end_time_ns":1700000002050000000,"events":[],"name":"GET /orders","parent_span_id":null,"service":"otlp-demo","span_id":"eee19b7ec3c1b174","start_time_ns":1700000002000000000,"status":"","trace_id":"5b8efff798038103d269b633813fc60c"}],"trace_id":"5b8efff798038103d269b633813fc60c"}
```

A successful protobuf request answers `200` with `Content-Type:
application/x-protobuf` and a zero-length body, which is the encoding of an
empty `ExportTraceServiceResponse`. A successful JSON request answers `200`
with `{"partialSuccess":{}}`.

### OTLP mapping

| OTLP | Traza span |
|---|---|
| `traceId` / `spanId` / `parentSpanId` | Lowercased hex string. Non-hex is a `400` |
| `startTimeUnixNano` / `endTimeUnixNano` | `start_time_ns` / `end_time_ns`. Accepted as a JSON string (OTLP JSON's u64 encoding) or a number |
| Resource attribute `service.name` | `service` |
| Resource attribute `traza.tenant` | The span's tenant, validated like any client-supplied one |
| **Every other resource attribute** | **Dropped** — see below |
| Scope attributes | Merged beneath span attributes, which take precedence on a collision |
| `attributes` (`AnyValue`) | Flattened to plain JSON: `intValue` becomes a number, `doubleValue` a double, `stringValue` a string |
| `status.code` `STATUS_CODE_OK` / `1` | `status: "ok"` |
| `status.code` `STATUS_CODE_ERROR` / `2` | `status: "error"` |
| Any other or absent status | `status: ""` |
| `events` | Span events, attributes flattened the same way |
| `links` | Span links, ids lowercased |

A link whose id pair is empty is dropped rather than treated as fatal — a link
to nothing is meaningless, and rejecting a whole batch for it would be worse.

#### Resource attributes do not reach the span

Two are read — `service.name` and `traza.tenant` — and **the rest are
discarded**. `deployment.environment`, `service.version`,
`service.instance.id`, `host.name`, `k8s.*` and `telemetry.sdk.*` are not
stored and cannot be filtered on. Scope attributes are different: those *are*
merged beneath the span's own.

If you need a resource attribute to be queryable, copy it onto spans — an SDK
span processor, or an OpenTelemetry Collector `transform` processor moving
`resource.attributes[...]` to `attributes[...]`. Span attributes round-trip
faithfully and are indexed like any other.

This page previously said other resource attributes were "merged beneath span
attributes". They never were.

`span.kind` has no dedicated Traza field. If you need it queryable, carry it as
an attribute.

### Error handling

A body that does not decode — malformed protobuf, malformed JSON, a non-hex id,
a timestamp that is not a `u64` — is a `400` with the reason. A store failure is
a `503`. There is no partial acceptance: `partialSuccess` is always empty
because a batch either lands entirely or is rejected entirely.

## LLM and agent telemetry

Both routes preserve `gen_ai.*`, `llm.*`, and `traceloop.*` attributes
verbatim, and Traza's derived views read them directly. An app instrumented
with OpenLLMetry or the OpenTelemetry GenAI conventions populates sessions,
provider and model rollups, and token/cost analytics with **no attribute
renaming** on your side.

The recognized keys, their precedence, and query recipes are in
[LLM semantics](../llm-semantics.md).

## Making writes durable

`POST /v1/flush` seals everything currently buffered into a segment:

```sh
curl -X POST http://localhost:8080/v1/flush
```

```json
{"flushed":true}
```

You rarely need this. Flushing happens automatically at `--flush-spans`
(10,000 by default), and in the default `wal` mode an acknowledged write is
already fsynced to the log before the response is sent — it survives a crash
whether or not it has been sealed. `POST /v1/flush` is useful in tests, before
a planned shutdown, or to force segment statistics to appear.

## Using the engine directly

The engine is a library; a process that generates its own spans can skip HTTP
entirely and get the same durability:

```rust
use traza::{Config, Span, SpanFilter, Store};

let store = Store::open("./data", Config::default())?;

store.ingest(Span {
    trace_id: "trace-1".into(),
    span_id: "span-1".into(),
    parent_span_id: None,
    name: "charge".into(),
    service: "checkout".into(),
    start_time_ns: 1_700_000_000_000_000_000,
    end_time_ns: 1_700_000_000_002_500_000,
    status: "ok".into(),
    attributes: Default::default(),
    events: Vec::new(),
    links: Vec::new(),
    extra: Default::default(),
})?;

store.flush()?;   // or let it flush automatically at the threshold

let slow = store.query(&SpanFilter {
    service: Some("checkout".into()),
    min_duration_ns: Some(2_000_000),
    ..SpanFilter::default()
})?;
```

`Store::ingest_batch` takes a `Vec<Span>` and is the batched form. A data
directory has exactly one writer, so an embedding process must not also run a
server against the same directory.
