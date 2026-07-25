//! OTLP/HTTP JSON ingest mapping, and the mapping rules both wire formats share.
//!
//! Parses an OpenTelemetry `ExportTraceServiceRequest` encoded as OTLP/HTTP
//! JSON and maps it onto [`crate::Span`]s per the documented conventions:
//! hex ids stored lowercased, `*TimeUnixNano` accepted as string or number
//! (OTLP JSON encodes u64 as string), typed `AnyValue` attributes flattened
//! to plain JSON, the resource attribute `service.name` becoming the span's
//! service, and scope attributes merging beneath span attributes.
//!
//! The request is deserialized STRAIGHT into these types rather than into a
//! `serde_json::Value` and walked afterwards. A `Value` round trip allocated a
//! `Map` and a `String` key for every envelope node — every `resourceSpans`,
//! `scopeSpans`, span, `KeyValue` and `AnyValue` wrapper — only to tear them
//! down again one call later. Leaf values are still typed `Value` on purpose:
//! it costs one allocation either way (a JSON string IS a `String`), and it
//! lets the lenient accessors below be shared verbatim, so the leniency the
//! wire formats promise cannot drift as the shapes change.
//!
//! [`crate::otlp_pb`] decodes the protobuf encoding into the SAME [`SpanParts`]
//! and [`AnyValueParts`] types, so the mapping decisions — attribute
//! precedence, status text, link filtering, the non-empty id rule — are made
//! exactly once for both encodings.

use crate::{Event, Link, Span};
use serde::de::{Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::fmt;

/// A structural problem in an OTLP request body.
#[derive(Debug)]
pub struct OtlpError(pub String);

impl fmt::Display for OtlpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid OTLP request: {}", self.0)
    }
}

impl std::error::Error for OtlpError {}

fn err(message: &str) -> OtlpError {
    OtlpError(message.to_owned())
}

// ------------------------------------------------- the shared mapping rules

/// One span's worth of decoded fields, before the OTLP mapping rules apply.
///
/// Both decoders fill this in and call [`SpanParts::finish`], which is the only
/// place that knows how OTLP becomes a [`Span`]. Two hand-written decoders that
/// must agree on semantics is a real correctness risk; sharing the decisions
/// rather than duplicating them is what keeps them from drifting, and
/// `otlp_json_and_protobuf_agree_on_one_payload` proves it on a payload that
/// exercises every `AnyValue` variant.
pub(crate) struct SpanParts {
    pub trace_id: String,
    pub span_id: String,
    /// Empty means "no parent" — both an absent field and an all-zero id.
    pub parent_span_id: String,
    pub name: String,
    pub start_time_ns: u64,
    pub end_time_ns: u64,
    pub status_code: Option<u64>,
    pub attributes: Map<String, Value>,
    pub events: Vec<Event>,
    pub links: Vec<Link>,
}

impl SpanParts {
    pub(crate) fn finish(
        self,
        service: &str,
        scope_attributes: &Map<String, Value>,
    ) -> Result<Span, OtlpError> {
        if self.trace_id.is_empty() || self.span_id.is_empty() {
            return Err(err("traceId and spanId must be non-empty"));
        }
        // Scope attributes sit BENEATH span attributes. When there are none —
        // the common case — the span's own map is already the answer, so it
        // moves through untouched instead of being re-inserted key by key
        // into a fresh map.
        let attributes = if scope_attributes.is_empty() {
            self.attributes
        } else {
            let mut merged = scope_attributes.clone();
            for (key, value) in self.attributes {
                merged.insert(key, value); // span attributes win on collision
            }
            merged
        };
        Ok(Span {
            trace_id: self.trace_id,
            span_id: self.span_id,
            parent_span_id: (!self.parent_span_id.is_empty()).then_some(self.parent_span_id),
            name: self.name,
            start_time_ns: self.start_time_ns,
            end_time_ns: self.end_time_ns,
            status: status_text(self.status_code),
            service: service.to_owned(),
            attributes,
            events: self.events,
            // Links carry the non-tree structure (fan-out/fan-in, retries); a
            // link with an empty id pair is meaningless and dropped rather
            // than fatal.
            links: self
                .links
                .into_iter()
                .filter(|link| !link.trace_id.is_empty() && !link.span_id.is_empty())
                .collect(),
            extra: Map::new(),
        })
    }
}

fn status_text(code: Option<u64>) -> String {
    match code {
        Some(1) => "ok".to_owned(),
        Some(2) => "error".to_owned(),
        _ => String::new(),
    }
}

/// The `AnyValue` variants a decoder found, resolved to a plain JSON value.
///
/// Kept as a struct of options rather than resolved on sight because the
/// precedence is fixed (string, int, double, bool, array, kvlist, bytes) and
/// must NOT depend on the order the fields happened to arrive in — JSON object
/// key order and protobuf field order are both arbitrary.
#[derive(Default)]
pub(crate) struct AnyValueParts {
    pub string: Option<Value>,
    pub int: Option<Value>,
    pub double: Option<Value>,
    pub boolean: Option<Value>,
    pub array: Option<Vec<Value>>,
    pub kvlist: Option<Map<String, Value>>,
    /// The raw bytes, NOT the form they are stored in. The two encodings put
    /// them on the wire differently — protobuf raw, proto3 JSON base64 — so
    /// each decoder undoes its own encoding and [`AnyValueParts::resolve`]
    /// alone decides what an attribute ends up holding.
    pub bytes: Option<Vec<u8>>,
}

impl AnyValueParts {
    /// Flattens to a plain JSON value.
    pub(crate) fn resolve(self) -> Value {
        if let Some(text) = self.string {
            return text;
        }
        if let Some(int) = self.int {
            // OTLP JSON encodes 64-bit ints as strings.
            return match int {
                Value::String(text) => text
                    .parse::<i64>()
                    .map(Value::from)
                    .unwrap_or(Value::String(text)),
                other => other,
            };
        }
        if let Some(double) = self.double {
            return double;
        }
        if let Some(boolean) = self.boolean {
            return boolean;
        }
        if let Some(values) = self.array {
            return Value::Array(values);
        }
        if let Some(values) = self.kvlist {
            return Value::Object(values);
        }
        if let Some(bytes) = self.bytes {
            // Lowercase hex, the way trace and span ids are stored. What an
            // attribute holds must not depend on which encoding delivered it,
            // and hex is the one representation this store already speaks —
            // so proto3 JSON's base64 is decoded on arrival rather than kept.
            return Value::String(hex(&bytes));
        }
        Value::Null
    }
}

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// Lowercase hex, by table. This was a `format!("{byte:02x}")` per byte — a
/// formatter invocation and a throwaway `String` for every byte of every trace
/// and span id, which on a 1M-span batch is 24 million of each.
pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX_DIGITS[usize::from(byte >> 4)] as char);
        out.push(HEX_DIGITS[usize::from(byte & 0x0f)] as char);
    }
    out
}

/// Normalizes a hex id in place: ids are stored lowercased, and anything that
/// is not hex is a client error rather than something to store and puzzle over.
fn hex_id(value: Option<Value>, field: &str) -> Result<String, OtlpError> {
    match value {
        None => Err(OtlpError(format!("{field} is required"))),
        Some(Value::String(mut text)) => {
            if text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                text.make_ascii_lowercase();
                Ok(text)
            } else {
                Err(OtlpError(format!("{field} must be hex")))
            }
        }
        Some(_) => Err(OtlpError(format!("{field} must be a hex string"))),
    }
}

fn nanos(value: Option<Value>, field: &str) -> Result<u64, OtlpError> {
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

/// A name field: absent or not a string reads as empty rather than fatal.
fn lenient_name(value: Option<Value>) -> String {
    match value {
        Some(Value::String(text)) => text,
        _ => String::new(),
    }
}

fn code_number(code: Option<Value>) -> Option<u64> {
    match code? {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => match text.as_str() {
            "STATUS_CODE_OK" => Some(1),
            "STATUS_CODE_ERROR" => Some(2),
            _ => text.parse().ok(),
        },
        _ => None,
    }
}

// ------------------------------------------------------------- JSON decoding

/// Maps an OTLP/HTTP JSON request body to spans.
pub fn spans_from_json(body: &[u8]) -> Result<Vec<Span>, OtlpError> {
    let request: JsonRequest =
        serde_json::from_slice(body).map_err(|error| OtlpError(error.to_string()))?;
    let mut spans = Vec::new();
    for resource_entry in request.resource_spans {
        let service = resource_entry
            .resource
            .and_then(|resource| resource.service_name())
            .unwrap_or_else(|| "unknown_service".to_owned());
        for scope_entry in resource_entry.scope_spans {
            let scope_attributes = scope_entry.scope.map(|scope| scope.attributes.0);
            let scope_attributes = scope_attributes.unwrap_or_default();
            for json_span in scope_entry.spans {
                spans.push(
                    json_span
                        .into_parts()?
                        .finish(&service, &scope_attributes)?,
                );
            }
        }
    }
    Ok(spans)
}

/// The envelope.
///
/// `resourceSpans`, `scopeSpans` and `spans` are REQUIRED arrays — the DOM walk
/// failed on those, and so does this. Everything else keeps the DOM walk's
/// leniency: it probed with `.and_then(Value::as_array)` and fell back to empty
/// rather than failing, so a malformed `attributes`, `events` or `links` still
/// drops quietly instead of rejecting the batch.
#[derive(Deserialize)]
struct JsonRequest {
    #[serde(rename = "resourceSpans")]
    resource_spans: Vec<JsonResourceSpans>,
}

#[derive(Deserialize)]
struct JsonResourceSpans {
    #[serde(default)]
    resource: Option<JsonResource>,
    #[serde(rename = "scopeSpans")]
    scope_spans: Vec<JsonScopeSpans>,
}

#[derive(Default)]
struct JsonResource {
    attributes: Attributes,
}

impl JsonResource {
    /// The FIRST `service.name` attribute whose value is a string. A
    /// non-string one does not stop the scan, exactly as the DOM walk's
    /// `find_map` did not.
    fn service_name(self) -> Option<String> {
        self.attributes
            .0
            .into_iter()
            .find_map(|(key, value)| match (key.as_str(), value) {
                ("service.name", Value::String(name)) => Some(name),
                _ => None,
            })
    }
}

#[derive(Deserialize)]
struct JsonScopeSpans {
    #[serde(default)]
    scope: Option<JsonScope>,
    spans: Vec<JsonSpan>,
}

#[derive(Default)]
struct JsonScope {
    attributes: Attributes,
}

#[derive(Deserialize)]
struct JsonSpan {
    #[serde(rename = "traceId", default)]
    trace_id: Option<Value>,
    #[serde(rename = "spanId", default)]
    span_id: Option<Value>,
    #[serde(rename = "parentSpanId", default)]
    parent_span_id: Option<Value>,
    #[serde(default)]
    name: Option<Value>,
    #[serde(rename = "startTimeUnixNano", default)]
    start_time_ns: Option<Value>,
    #[serde(rename = "endTimeUnixNano", default)]
    end_time_ns: Option<Value>,
    #[serde(default)]
    status: Option<JsonStatus>,
    #[serde(default)]
    attributes: Attributes,
    #[serde(default)]
    events: LenientSeq<JsonEvent>,
    #[serde(default)]
    links: LenientSeq<JsonLink>,
}

impl JsonSpan {
    fn into_parts(self) -> Result<SpanParts, OtlpError> {
        Ok(SpanParts {
            trace_id: hex_id(self.trace_id, "traceId")?,
            span_id: hex_id(self.span_id, "spanId")?,
            parent_span_id: match self.parent_span_id {
                None | Some(Value::Null) => String::new(),
                parent => hex_id(parent, "parentSpanId")?,
            },
            name: lenient_name(self.name),
            start_time_ns: nanos(self.start_time_ns, "startTimeUnixNano")?,
            end_time_ns: nanos(self.end_time_ns, "endTimeUnixNano")?,
            status_code: self.status.and_then(|status| code_number(status.code)),
            attributes: self.attributes.0,
            events: self
                .events
                .0
                .into_iter()
                .map(|event| {
                    Ok(Event {
                        name: lenient_name(event.name),
                        timestamp_ns: nanos(event.timestamp_ns, "event timeUnixNano")?,
                        attributes: event.attributes.0,
                    })
                })
                .collect::<Result<Vec<_>, OtlpError>>()?,
            links: self
                .links
                .0
                .into_iter()
                .map(|link| {
                    Ok(Link {
                        trace_id: hex_id(link.trace_id, "link traceId")?,
                        span_id: hex_id(link.span_id, "link spanId")?,
                        attributes: link.attributes.0,
                    })
                })
                .collect::<Result<Vec<_>, OtlpError>>()?,
        })
    }
}

#[derive(Deserialize)]
struct JsonStatus {
    #[serde(default)]
    code: Option<Value>,
}

#[derive(Deserialize)]
struct JsonEvent {
    #[serde(default)]
    name: Option<Value>,
    #[serde(rename = "timeUnixNano", default)]
    timestamp_ns: Option<Value>,
    #[serde(default)]
    attributes: Attributes,
}

#[derive(Deserialize)]
struct JsonLink {
    #[serde(rename = "traceId", default)]
    trace_id: Option<Value>,
    #[serde(rename = "spanId", default)]
    span_id: Option<Value>,
    #[serde(default)]
    attributes: Attributes,
}

// ------------------------------------- AnyValue / KeyValue streaming visitors

/// Emits the visitor arms that make a SHAPE MISMATCH fall back to a default
/// instead of failing the whole request.
///
/// The DOM walk probed with `.get(..).and_then(Value::as_array)`, which yielded
/// `None` — and therefore an empty collection — for anything unexpected. These
/// arms reproduce that. They must consume whatever they are handed: returning
/// early without draining a map or sequence would leave the JSON parser
/// mid-value and corrupt everything after it.
macro_rules! lenient_fallbacks {
    ($produced:ty, $default:expr) => {
        fn visit_unit<E: serde::de::Error>(self) -> Result<$produced, E> {
            Ok($default)
        }
        fn visit_none<E: serde::de::Error>(self) -> Result<$produced, E> {
            Ok($default)
        }
        fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<$produced, E> {
            Ok($default)
        }
        fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<$produced, E> {
            Ok($default)
        }
        fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<$produced, E> {
            Ok($default)
        }
        fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<$produced, E> {
            Ok($default)
        }
        fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<$produced, E> {
            Ok($default)
        }
    };
}

/// Drains a map that is being ignored, so the parser stays in step.
fn drain_map<'de, A: MapAccess<'de>>(mut map: A) -> Result<(), A::Error> {
    while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
    Ok(())
}

/// Drains a sequence that is being ignored, so the parser stays in step.
fn drain_seq<'de, A: SeqAccess<'de>>(mut seq: A) -> Result<(), A::Error> {
    while seq.next_element::<IgnoredAny>()?.is_some() {}
    Ok(())
}

/// An OTLP `KeyValue` array, flattened to a plain map as it is read.
#[derive(Default)]
struct Attributes(Map<String, Value>);

impl<'de> Deserialize<'de> for Attributes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct AttributesVisitor;
        impl<'de> Visitor<'de> for AttributesVisitor {
            type Value = Attributes;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an array of OTLP KeyValue objects")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Attributes, A::Error> {
                let mut map = Map::new();
                while let Some(entry) = seq.next_element::<KeyValue>()? {
                    // A non-string key, or an entry with no `value` field at
                    // all, is skipped rather than fatal — as the DOM walk did.
                    if let (Some(key), Presence::Present(value)) = (entry.key, entry.value) {
                        map.insert(key.0, value);
                    }
                }
                Ok(Attributes(map))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Attributes, A::Error> {
                drain_map(map)?;
                Ok(Attributes::default())
            }
            fn visit_some<D: Deserializer<'de>>(self, inner: D) -> Result<Attributes, D::Error> {
                Attributes::deserialize(inner)
            }
            lenient_fallbacks!(Attributes, Attributes::default());
        }
        deserializer.deserialize_any(AttributesVisitor)
    }
}

/// A sequence of `T`, or empty for any other shape.
struct LenientSeq<T>(Vec<T>);

impl<T> Default for LenientSeq<T> {
    fn default() -> Self {
        LenientSeq(Vec::new())
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for LenientSeq<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SeqVisitor<T>(std::marker::PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for SeqVisitor<T> {
            type Value = LenientSeq<T>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an array")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<LenientSeq<T>, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element::<T>()? {
                    items.push(item);
                }
                Ok(LenientSeq(items))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<LenientSeq<T>, A::Error> {
                drain_map(map)?;
                Ok(LenientSeq::default())
            }
            fn visit_some<D: Deserializer<'de>>(self, inner: D) -> Result<LenientSeq<T>, D::Error> {
                LenientSeq::deserialize(inner)
            }
            lenient_fallbacks!(LenientSeq<T>, LenientSeq::default());
        }
        deserializer.deserialize_any(SeqVisitor(std::marker::PhantomData))
    }
}

/// An object carrying only `attributes` — `resource` and `scope`. Any other
/// shape reads as "no attributes", which is how the DOM walk treated it.
macro_rules! attributes_only_object {
    ($name:ident, $expecting:literal) => {
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct ObjectVisitor;
                impl<'de> Visitor<'de> for ObjectVisitor {
                    type Value = $name;
                    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        f.write_str($expecting)
                    }
                    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<$name, A::Error> {
                        let mut attributes = Attributes::default();
                        while let Some(key) = map.next_key::<String>()? {
                            if key == "attributes" {
                                attributes = map.next_value()?;
                            } else {
                                map.next_value::<IgnoredAny>()?;
                            }
                        }
                        Ok($name { attributes })
                    }
                    fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<$name, A::Error> {
                        drain_seq(seq)?;
                        Ok($name::default())
                    }
                    fn visit_some<D: Deserializer<'de>>(self, inner: D) -> Result<$name, D::Error> {
                        $name::deserialize(inner)
                    }
                    lenient_fallbacks!($name, $name::default());
                }
                deserializer.deserialize_any(ObjectVisitor)
            }
        }
    };
}

attributes_only_object!(JsonResource, "an OTLP Resource");
attributes_only_object!(JsonScope, "an OTLP InstrumentationScope");

#[derive(Deserialize, Default)]
struct KeyValue {
    #[serde(default)]
    key: Option<StringKey>,
    #[serde(default)]
    value: Presence,
}

/// `Option<String>` would make a non-string key a hard error; the DOM walk
/// skipped such an entry, so anything that is not a string reads as absent.
struct StringKey(String);

impl<'de> Deserialize<'de> for StringKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct KeyVisitor;
        impl<'de> Visitor<'de> for KeyVisitor {
            type Value = Option<StringKey>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an attribute key")
            }
            fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<Self::Value, E> {
                Ok(Some(StringKey(text.to_owned())))
            }
            fn visit_string<E: serde::de::Error>(self, text: String) -> Result<Self::Value, E> {
                Ok(Some(StringKey(text)))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Self::Value, A::Error> {
                drain_map(map)?;
                Ok(None)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
                drain_seq(seq)?;
                Ok(None)
            }
            fn visit_some<D: Deserializer<'de>>(self, inner: D) -> Result<Self::Value, D::Error> {
                inner.deserialize_any(KeyVisitor)
            }
            // Not `lenient_fallbacks!`: this visitor defines its own
            // `visit_str`, which is the one shape that yields a key.
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(None)
            }
            fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(None)
            }
        }
        // A non-string key must read as absent, not as an error. `Option` is
        // the carrier for that, so the outer field is `Option<StringKey>` and
        // this returns the empty string only when serde demands a StringKey.
        Ok(deserializer
            .deserialize_any(KeyVisitor)?
            .unwrap_or(StringKey(String::new())))
    }
}

/// Distinguishes "no `value` field" from "`value: null`". `Option<Value>`
/// cannot: serde maps a JSON null onto `None`, which would silently drop an
/// attribute the DOM walk kept as a null.
#[derive(Default)]
enum Presence {
    #[default]
    Absent,
    Present(Value),
}

impl<'de> Deserialize<'de> for Presence {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Presence::Present(any_value(deserializer)?))
    }
}

/// Reads one `AnyValue`. Anything that is not an object — including null —
/// flattens to null, matching the DOM walk probing a non-object with
/// `.get("stringValue")` and finding nothing.
fn any_value<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Value, D::Error> {
    struct AnyValueVisitor;
    impl<'de> Visitor<'de> for AnyValueVisitor {
        type Value = Value;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("an OTLP AnyValue")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
            let mut parts = AnyValueParts::default();
            while let Some(key) = map.next_key::<String>()? {
                match key.as_str() {
                    "stringValue" => parts.string = Some(map.next_value()?),
                    "intValue" => parts.int = Some(map.next_value()?),
                    "doubleValue" => parts.double = Some(map.next_value()?),
                    "boolValue" => parts.boolean = Some(map.next_value()?),
                    "arrayValue" => parts.array = Some(map.next_value::<ArrayValue>()?.0),
                    "kvlistValue" => parts.kvlist = Some(map.next_value::<KvListValue>()?.0),
                    "bytesValue" => parts.bytes = map.next_value::<Base64Bytes>()?.0,
                    // A key no variant claims — a newer `AnyValue` member, say.
                    _ => {
                        map.next_value::<IgnoredAny>()?;
                    }
                }
            }
            Ok(parts.resolve())
        }
        fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Value, A::Error> {
            drain_seq(seq)?;
            Ok(Value::Null)
        }
        fn visit_some<D: Deserializer<'de>>(self, inner: D) -> Result<Value, D::Error> {
            any_value(inner)
        }
        lenient_fallbacks!(Value, Value::Null);
    }
    deserializer.deserialize_any(AnyValueVisitor)
}

/// A proto3-JSON `bytesValue`: base64 text. Anything else — a non-string
/// shape, or text that is not base64 — reads as absent, so the attribute
/// resolves to null the way every other shape mismatch in this module falls
/// back to its default.
struct Base64Bytes(Option<Vec<u8>>);

impl<'de> Deserialize<'de> for Base64Bytes {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct BytesVisitor;
        impl<'de> Visitor<'de> for BytesVisitor {
            type Value = Base64Bytes;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("base64-encoded bytes")
            }
            fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<Base64Bytes, E> {
                Ok(Base64Bytes(base64_decode(text)))
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Base64Bytes, A::Error> {
                drain_map(map)?;
                Ok(Base64Bytes(None))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<Base64Bytes, A::Error> {
                drain_seq(seq)?;
                Ok(Base64Bytes(None))
            }
            fn visit_some<D: Deserializer<'de>>(self, inner: D) -> Result<Base64Bytes, D::Error> {
                Base64Bytes::deserialize(inner)
            }
            // Not `lenient_fallbacks!`: this visitor defines its own
            // `visit_str`, which is the one shape that yields bytes.
            fn visit_unit<E: serde::de::Error>(self) -> Result<Base64Bytes, E> {
                Ok(Base64Bytes(None))
            }
            fn visit_none<E: serde::de::Error>(self) -> Result<Base64Bytes, E> {
                Ok(Base64Bytes(None))
            }
            fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Base64Bytes, E> {
                Ok(Base64Bytes(None))
            }
            fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Base64Bytes, E> {
                Ok(Base64Bytes(None))
            }
            fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Base64Bytes, E> {
                Ok(Base64Bytes(None))
            }
            fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Base64Bytes, E> {
                Ok(Base64Bytes(None))
            }
        }
        deserializer.deserialize_any(BytesVisitor)
    }
}

/// Decodes proto3 JSON's `bytes` encoding, or `None` when the text is not
/// base64 at all.
///
/// Both the standard (`+/`) and the URL-safe (`-_`) alphabet are accepted,
/// padded or not, which is the range protobuf's own JSON parsers accept. An
/// exporter that picks any of them means the same bytes, and must not lose the
/// attribute over the choice.
fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut accumulator: u32 = 0;
    let mut bits: u32 = 0;
    let mut padded = false;
    for byte in text.bytes() {
        if byte == b'=' {
            // Padding only ever ends a stream. Data after it would be two
            // encodings concatenated, which is not one value.
            padded = true;
            continue;
        }
        if padded {
            return None;
        }
        let sextet = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        };
        accumulator = (accumulator << 6) | u32::from(sextet);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accumulator >> bits) as u8);
        }
    }
    // Six leftover bits are a group of one character: it carries no whole
    // byte, so the input was cut mid-value rather than merely left unpadded.
    (bits != 6).then_some(out)
}

/// The `values` array of an `ArrayValue`/`KeyValueList`, or empty for any
/// other shape.
macro_rules! values_wrapper {
    ($name:ident, $inner:ty, $expecting:literal) => {
        struct $name($inner);
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct WrapperVisitor;
                impl<'de> Visitor<'de> for WrapperVisitor {
                    type Value = $name;
                    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        f.write_str($expecting)
                    }
                    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<$name, A::Error> {
                        let mut values = <$inner>::default();
                        while let Some(key) = map.next_key::<String>()? {
                            if key == "values" {
                                values = map.next_value()?;
                            } else {
                                map.next_value::<IgnoredAny>()?;
                            }
                        }
                        Ok($name(values))
                    }
                    fn visit_seq<A: SeqAccess<'de>>(self, seq: A) -> Result<$name, A::Error> {
                        drain_seq(seq)?;
                        Ok($name(<$inner>::default()))
                    }
                    fn visit_some<D: Deserializer<'de>>(self, inner: D) -> Result<$name, D::Error> {
                        $name::deserialize(inner)
                    }
                    lenient_fallbacks!($name, $name(<$inner>::default()));
                }
                deserializer.deserialize_any(WrapperVisitor)
            }
        }
    };
}

values_wrapper!(
    ArrayValueInner,
    LenientSeq<AnyValueItem>,
    "an OTLP ArrayValue"
);
values_wrapper!(KvListInner, Attributes, "an OTLP KeyValueList");

struct AnyValueItem(Value);

impl<'de> Deserialize<'de> for AnyValueItem {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(AnyValueItem(any_value(deserializer)?))
    }
}

struct ArrayValue(Vec<Value>);

impl<'de> Deserialize<'de> for ArrayValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(ArrayValue(
            ArrayValueInner::deserialize(deserializer)?
                .0
                 .0
                .into_iter()
                .map(|item| item.0)
                .collect(),
        ))
    }
}

struct KvListValue(Map<String, Value>);

impl<'de> Deserialize<'de> for KvListValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(KvListValue(KvListInner::deserialize(deserializer)?.0 .0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One span carrying one attribute, so a single `AnyValue` can be mapped
    /// through the real entry point rather than through the visitor directly.
    fn attribute(value: Value) -> Value {
        let span = spans_from_json(
            serde_json::json!({"resourceSpans": [{"scopeSpans": [{"spans": [{
                "traceId": "aa", "spanId": "bb",
                "startTimeUnixNano": "1", "endTimeUnixNano": "2",
                "attributes": [{"key": "a", "value": value}]
            }]}]}]})
            .to_string()
            .as_bytes(),
        )
        .expect("maps")
        .remove(0);
        span.attributes["a"].clone()
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_decode(""), Some(Vec::new()));
        assert_eq!(base64_decode("Zg=="), Some(b"f".to_vec()));
        assert_eq!(base64_decode("Zm8="), Some(b"fo".to_vec()));
        assert_eq!(base64_decode("Zm9v"), Some(b"foo".to_vec()));
        assert_eq!(base64_decode("Zm9vYmFy"), Some(b"foobar".to_vec()));
    }

    /// Padding is optional and either alphabet is fine: exporters differ, and
    /// they all mean the same bytes.
    #[test]
    fn base64_accepts_unpadded_and_url_safe_text() {
        assert_eq!(base64_decode("Zg"), Some(b"f".to_vec()));
        assert_eq!(base64_decode("Zm8"), Some(b"fo".to_vec()));
        assert_eq!(base64_decode("+/+/"), Some(vec![0xfb, 0xff, 0xbf]));
        assert_eq!(base64_decode("-_-_"), Some(vec![0xfb, 0xff, 0xbf]));
    }

    #[test]
    fn base64_rejects_text_that_is_not_base64() {
        assert_eq!(base64_decode("Z"), None, "a lone character is truncated");
        assert_eq!(base64_decode("Zm9v!"), None, "outside the alphabet");
        assert_eq!(base64_decode("Zm 9v"), None, "whitespace is not skipped");
        assert_eq!(base64_decode("Zg==Zg=="), None, "data after the padding");
    }

    #[test]
    fn base64_round_trips_every_byte() {
        let all: Vec<u8> = (0..=255).collect();
        for length in 0..all.len() {
            let bytes = &all[..length];
            assert_eq!(
                base64_decode(&crate::media::base64_encode(bytes)).as_deref(),
                Some(bytes)
            );
        }
    }

    #[test]
    fn bytes_attributes_are_stored_as_hex() {
        assert_eq!(
            attribute(serde_json::json!({"bytesValue": "AQID"})),
            Value::String("010203".to_owned())
        );
        assert_eq!(
            attribute(serde_json::json!({"bytesValue": ""})),
            Value::String(String::new()),
            "empty bytes are empty, not absent"
        );
    }

    /// A `bytesValue` that is not base64 is not bytes; it reads as null rather
    /// than failing the batch, which is how this module treats every other
    /// value it cannot make sense of.
    #[test]
    fn unreadable_bytes_attributes_read_as_null() {
        assert_eq!(
            attribute(serde_json::json!({"bytesValue": "!!"})),
            Value::Null
        );
        assert_eq!(attribute(serde_json::json!({"bytesValue": 7})), Value::Null);
        assert_eq!(
            attribute(serde_json::json!({"bytesValue": {"nested": true}})),
            Value::Null
        );
    }

    /// Precedence must not depend on key order: `stringValue` outranks
    /// `bytesValue` whichever one the encoder wrote first.
    #[test]
    fn string_outranks_bytes_either_way_round() {
        assert_eq!(
            attribute(serde_json::json!({"stringValue": "s", "bytesValue": "AQID"})),
            Value::String("s".to_owned())
        );
        assert_eq!(
            attribute(serde_json::json!({"bytesValue": "AQID", "stringValue": "s"})),
            Value::String("s".to_owned())
        );
    }
}
