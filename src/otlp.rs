//! OTLP/HTTP JSON ingest mapping.
//!
//! Parses an OpenTelemetry `ExportTraceServiceRequest` encoded as OTLP/HTTP
//! JSON and maps it onto [`crate::Span`]s per the documented conventions:
//! hex ids stored lowercased, `*TimeUnixNano` accepted as string or number
//! (OTLP JSON encodes u64 as string), typed `AnyValue` attributes flattened
//! to plain JSON, the resource attribute `service.name` becoming the span's
//! service, and scope attributes merging beneath span attributes.

use crate::{Event, Span};
use serde_json::{Map, Value};

/// A structural problem in an OTLP request body.
#[derive(Debug)]
pub struct OtlpError(pub String);

impl std::fmt::Display for OtlpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid OTLP request: {}", self.0)
    }
}

fn err(message: &str) -> OtlpError {
    OtlpError(message.to_owned())
}

/// Maps a parsed OTLP JSON request to spans. The caller parses the body so
/// malformed JSON and structural errors surface distinctly.
pub fn spans_from_request(request: &Value) -> Result<Vec<Span>, OtlpError> {
    let resource_spans = request
        .get("resourceSpans")
        .and_then(Value::as_array)
        .ok_or_else(|| err("resourceSpans array is required"))?;
    let mut spans = Vec::new();
    for resource_entry in resource_spans {
        let service = resource_entry
            .get("resource")
            .and_then(|resource| resource.get("attributes"))
            .and_then(Value::as_array)
            .and_then(|attributes| {
                attributes.iter().find_map(|attribute| {
                    if attribute.get("key")?.as_str()? != "service.name" {
                        return None;
                    }
                    any_value(attribute.get("value")?)
                        .as_str()
                        .map(str::to_owned)
                })
            })
            .unwrap_or_else(|| "unknown_service".to_owned());
        let scope_spans = resource_entry
            .get("scopeSpans")
            .and_then(Value::as_array)
            .ok_or_else(|| err("scopeSpans array is required"))?;
        for scope_entry in scope_spans {
            let scope_attributes =
                attribute_map(scope_entry.get("scope").and_then(|s| s.get("attributes")));
            let otlp_spans = scope_entry
                .get("spans")
                .and_then(Value::as_array)
                .ok_or_else(|| err("spans array is required"))?;
            for otlp_span in otlp_spans {
                spans.push(map_span(otlp_span, &service, &scope_attributes)?);
            }
        }
    }
    Ok(spans)
}

fn map_span(
    otlp: &Value,
    service: &str,
    scope_attributes: &Map<String, Value>,
) -> Result<Span, OtlpError> {
    let trace_id = hex_id(otlp.get("traceId"), "traceId")?;
    let span_id = hex_id(otlp.get("spanId"), "spanId")?;
    if trace_id.is_empty() || span_id.is_empty() {
        return Err(err("traceId and spanId must be non-empty"));
    }
    let parent = match otlp.get("parentSpanId") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let parent = hex_id(Some(value), "parentSpanId")?;
            (!parent.is_empty()).then_some(parent)
        }
    };
    let mut attributes = scope_attributes.clone();
    for (key, value) in attribute_map(otlp.get("attributes")) {
        attributes.insert(key, value); // span attributes win on collision
    }
    let events = otlp
        .get("events")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .map(|entry| {
                    Ok(Event {
                        name: entry
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        timestamp_ns: nanos(entry.get("timeUnixNano"), "event timeUnixNano")?,
                        attributes: attribute_map(entry.get("attributes")),
                    })
                })
                .collect::<Result<Vec<_>, OtlpError>>()
        })
        .transpose()?
        .unwrap_or_default();
    Ok(Span {
        trace_id,
        span_id,
        parent_span_id: parent,
        name: otlp
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        start_time_ns: nanos(otlp.get("startTimeUnixNano"), "startTimeUnixNano")?,
        end_time_ns: nanos(otlp.get("endTimeUnixNano"), "endTimeUnixNano")?,
        status: status_string(otlp.get("status")),
        service: service.to_owned(),
        attributes,
        events,
        extra: Map::new(),
    })
}

fn status_string(status: Option<&Value>) -> String {
    match status
        .and_then(|status| status.get("code"))
        .and_then(code_number)
    {
        Some(1) => "ok".to_owned(),
        Some(2) => "error".to_owned(),
        _ => String::new(),
    }
}

fn code_number(code: &Value) -> Option<u64> {
    match code {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => match text.as_str() {
            "STATUS_CODE_OK" => Some(1),
            "STATUS_CODE_ERROR" => Some(2),
            _ => text.parse().ok(),
        },
        _ => None,
    }
}

fn hex_id(value: Option<&Value>, field: &str) -> Result<String, OtlpError> {
    match value {
        None => Err(OtlpError(format!("{field} is required"))),
        Some(Value::String(text)) => {
            if text.chars().all(|c| c.is_ascii_hexdigit()) {
                Ok(text.to_ascii_lowercase())
            } else {
                Err(OtlpError(format!("{field} must be hex")))
            }
        }
        Some(_) => Err(OtlpError(format!("{field} must be a hex string"))),
    }
}

fn nanos(value: Option<&Value>, field: &str) -> Result<u64, OtlpError> {
    match value {
        Some(Value::String(text)) => text
            .parse()
            .map_err(|_| OtlpError(format!("{field} must be u64"))),
        Some(Value::Number(number)) => number
            .as_u64()
            .ok_or_else(|| OtlpError(format!("{field} must be u64"))),
        _ => Err(OtlpError(format!("{field} is required"))),
    }
}

fn attribute_map(attributes: Option<&Value>) -> Map<String, Value> {
    let mut map = Map::new();
    if let Some(entries) = attributes.and_then(Value::as_array) {
        for entry in entries {
            if let (Some(key), Some(value)) =
                (entry.get("key").and_then(Value::as_str), entry.get("value"))
            {
                map.insert(key.to_owned(), any_value(value));
            }
        }
    }
    map
}

/// Flattens an OTLP `AnyValue` to a plain JSON value.
fn any_value(value: &Value) -> Value {
    if let Some(text) = value.get("stringValue") {
        return text.clone();
    }
    if let Some(int) = value.get("intValue") {
        // OTLP JSON encodes 64-bit ints as strings.
        return match int {
            Value::String(text) => text
                .parse::<i64>()
                .map(Value::from)
                .unwrap_or_else(|_| int.clone()),
            other => other.clone(),
        };
    }
    if let Some(double) = value.get("doubleValue") {
        return double.clone();
    }
    if let Some(boolean) = value.get("boolValue") {
        return boolean.clone();
    }
    if let Some(array) = value.get("arrayValue") {
        let values = array
            .get("values")
            .and_then(Value::as_array)
            .map(|entries| entries.iter().map(any_value).collect())
            .unwrap_or_default();
        return Value::Array(values);
    }
    if let Some(kvlist) = value.get("kvlistValue") {
        return Value::Object(attribute_map(kvlist.get("values")));
    }
    Value::Null
}
