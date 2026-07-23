//! OTLP/HTTP binary-protobuf decoding, dependency-free.
//!
//! A hand-rolled protobuf wire decoder for the OTLP `TracesData` /
//! `ExportTraceServiceRequest` message (they share field 1:
//! `repeated ResourceSpans`). Rather than duplicating the OTLP-to-span
//! mapping, the decoder lowers protobuf into EXACTLY the JSON `Value` shape
//! of OTLP/HTTP JSON (camelCase keys, lowercase-hex ids, numeric
//! `timeUnixNano`), then hands off to [`crate::otlp::spans_from_request`] —
//! one mapping, two encodings, and every JSON-path conformance behavior
//! applies to protobuf automatically.
//!
//! Wire-format subset: varint (0), fixed64 (1), length-delimited (2),
//! fixed32 (5). Unknown fields are skipped by wire type, as the format
//! requires. All slicing is bounds-checked; malformed input yields
//! `DecodeError`, never a panic.

use serde_json::{json, Map, Value};

/// A malformed protobuf payload (truncated varint, bad length, wrong type).
#[derive(Debug)]
pub struct DecodeError(pub String);

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "protobuf decode error: {}", self.0)
    }
}

impl std::error::Error for DecodeError {}

fn err(message: impl Into<String>) -> DecodeError {
    DecodeError(message.into())
}

/// Nested kvlist/array depth cap: deeper input is hostile, not telemetry.
const MAX_VALUE_DEPTH: u32 = 32;

type Decoded<T> = Result<T, DecodeError>;

// ------------------------------------------------------------- wire reader

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn done(&self) -> bool {
        self.position >= self.bytes.len()
    }

    fn varint(&mut self) -> Decoded<u64> {
        let mut value: u64 = 0;
        for shift in 0..10 {
            let byte = *self
                .bytes
                .get(self.position)
                .ok_or_else(|| err("truncated varint"))?;
            self.position += 1;
            value |= u64::from(byte & 0x7f) << (shift * 7);
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(err("varint longer than 10 bytes"))
    }

    fn fixed64(&mut self) -> Decoded<u64> {
        let end = self
            .position
            .checked_add(8)
            .filter(|&end| end <= self.bytes.len())
            .ok_or_else(|| err("truncated fixed64"))?;
        let mut buffer = [0_u8; 8];
        buffer.copy_from_slice(&self.bytes[self.position..end]);
        self.position = end;
        Ok(u64::from_le_bytes(buffer))
    }

    fn fixed32(&mut self) -> Decoded<u32> {
        let end = self
            .position
            .checked_add(4)
            .filter(|&end| end <= self.bytes.len())
            .ok_or_else(|| err("truncated fixed32"))?;
        let mut buffer = [0_u8; 4];
        buffer.copy_from_slice(&self.bytes[self.position..end]);
        self.position = end;
        Ok(u32::from_le_bytes(buffer))
    }

    fn bytes_field(&mut self) -> Decoded<&'a [u8]> {
        let length = usize::try_from(self.varint()?).map_err(|_| err("length overflows"))?;
        let end = self
            .position
            .checked_add(length)
            .filter(|&end| end <= self.bytes.len())
            .ok_or_else(|| err("length past end of buffer"))?;
        let slice = &self.bytes[self.position..end];
        self.position = end;
        Ok(slice)
    }

    /// Reads one field tag; returns (field_number, wire_type).
    fn tag(&mut self) -> Decoded<(u64, u8)> {
        let key = self.varint()?;
        Ok((key >> 3, (key & 0x7) as u8))
    }

    fn skip(&mut self, wire_type: u8) -> Decoded<()> {
        match wire_type {
            0 => {
                self.varint()?;
            }
            1 => {
                self.fixed64()?;
            }
            2 => {
                self.bytes_field()?;
            }
            5 => {
                self.fixed32()?;
            }
            other => return Err(err(format!("unsupported wire type {other}"))),
        }
        Ok(())
    }
}

fn utf8(bytes: &[u8], what: &str) -> Decoded<String> {
    String::from_utf8(bytes.to_vec()).map_err(|_| err(format!("{what} is not UTF-8")))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

// -------------------------------------------------------- message decoders

/// Decodes an `ExportTraceServiceRequest` / `TracesData` payload into the
/// OTLP/HTTP JSON `Value` shape consumed by [`crate::otlp::spans_from_request`].
pub fn traces_request_to_json(bytes: &[u8]) -> Decoded<Value> {
    let mut resource_spans = Vec::new();
    let mut reader = Reader::new(bytes);
    while !reader.done() {
        let (field, wire_type) = reader.tag()?;
        match (field, wire_type) {
            (1, 2) => resource_spans.push(decode_resource_spans(reader.bytes_field()?)?),
            _ => reader.skip(wire_type)?,
        }
    }
    Ok(json!({ "resourceSpans": resource_spans }))
}

fn decode_resource_spans(bytes: &[u8]) -> Decoded<Value> {
    let mut resource = Value::Null;
    let mut scope_spans = Vec::new();
    let mut reader = Reader::new(bytes);
    while !reader.done() {
        let (field, wire_type) = reader.tag()?;
        match (field, wire_type) {
            (1, 2) => resource = decode_resource(reader.bytes_field()?)?,
            (2, 2) => scope_spans.push(decode_scope_spans(reader.bytes_field()?)?),
            _ => reader.skip(wire_type)?,
        }
    }
    let mut out = Map::new();
    if !resource.is_null() {
        out.insert("resource".into(), resource);
    }
    out.insert("scopeSpans".into(), Value::Array(scope_spans));
    Ok(Value::Object(out))
}

fn decode_resource(bytes: &[u8]) -> Decoded<Value> {
    let mut attributes = Vec::new();
    let mut reader = Reader::new(bytes);
    while !reader.done() {
        let (field, wire_type) = reader.tag()?;
        match (field, wire_type) {
            (1, 2) => attributes.push(decode_key_value(reader.bytes_field()?, 0)?),
            _ => reader.skip(wire_type)?,
        }
    }
    Ok(json!({ "attributes": attributes }))
}

fn decode_scope_spans(bytes: &[u8]) -> Decoded<Value> {
    let mut scope = Value::Null;
    let mut spans = Vec::new();
    let mut reader = Reader::new(bytes);
    while !reader.done() {
        let (field, wire_type) = reader.tag()?;
        match (field, wire_type) {
            (1, 2) => scope = decode_scope(reader.bytes_field()?)?,
            (2, 2) => spans.push(decode_span(reader.bytes_field()?)?),
            _ => reader.skip(wire_type)?,
        }
    }
    let mut out = Map::new();
    if !scope.is_null() {
        out.insert("scope".into(), scope);
    }
    out.insert("spans".into(), Value::Array(spans));
    Ok(Value::Object(out))
}

fn decode_scope(bytes: &[u8]) -> Decoded<Value> {
    let mut name = String::new();
    let mut attributes = Vec::new();
    let mut reader = Reader::new(bytes);
    while !reader.done() {
        let (field, wire_type) = reader.tag()?;
        match (field, wire_type) {
            (1, 2) => name = utf8(reader.bytes_field()?, "scope name")?,
            (3, 2) => attributes.push(decode_key_value(reader.bytes_field()?, 0)?),
            _ => reader.skip(wire_type)?,
        }
    }
    Ok(json!({ "name": name, "attributes": attributes }))
}

fn decode_span(bytes: &[u8]) -> Decoded<Value> {
    let mut out = Map::new();
    let mut attributes = Vec::new();
    let mut events = Vec::new();
    let mut links = Vec::new();
    let mut reader = Reader::new(bytes);
    while !reader.done() {
        let (field, wire_type) = reader.tag()?;
        match (field, wire_type) {
            (1, 2) => {
                out.insert("traceId".into(), Value::String(hex(reader.bytes_field()?)));
            }
            (2, 2) => {
                out.insert("spanId".into(), Value::String(hex(reader.bytes_field()?)));
            }
            (4, 2) => {
                out.insert(
                    "parentSpanId".into(),
                    Value::String(hex(reader.bytes_field()?)),
                );
            }
            (5, 2) => {
                out.insert(
                    "name".into(),
                    Value::String(utf8(reader.bytes_field()?, "span name")?),
                );
            }
            (7, 1) => {
                out.insert("startTimeUnixNano".into(), Value::from(reader.fixed64()?));
            }
            (8, 1) => {
                out.insert("endTimeUnixNano".into(), Value::from(reader.fixed64()?));
            }
            (9, 2) => attributes.push(decode_key_value(reader.bytes_field()?, 0)?),
            (11, 2) => events.push(decode_event(reader.bytes_field()?)?),
            (13, 2) => links.push(decode_link(reader.bytes_field()?)?),
            (15, 2) => {
                out.insert("status".into(), decode_status(reader.bytes_field()?)?);
            }
            _ => reader.skip(wire_type)?,
        }
    }
    out.insert("attributes".into(), Value::Array(attributes));
    if !events.is_empty() {
        out.insert("events".into(), Value::Array(events));
    }
    if !links.is_empty() {
        out.insert("links".into(), Value::Array(links));
    }
    Ok(Value::Object(out))
}

fn decode_event(bytes: &[u8]) -> Decoded<Value> {
    let mut out = Map::new();
    let mut attributes = Vec::new();
    let mut reader = Reader::new(bytes);
    while !reader.done() {
        let (field, wire_type) = reader.tag()?;
        match (field, wire_type) {
            (1, 1) => {
                out.insert("timeUnixNano".into(), Value::from(reader.fixed64()?));
            }
            (2, 2) => {
                out.insert(
                    "name".into(),
                    Value::String(utf8(reader.bytes_field()?, "event name")?),
                );
            }
            (3, 2) => attributes.push(decode_key_value(reader.bytes_field()?, 0)?),
            _ => reader.skip(wire_type)?,
        }
    }
    out.insert("attributes".into(), Value::Array(attributes));
    Ok(Value::Object(out))
}

fn decode_link(bytes: &[u8]) -> Decoded<Value> {
    let mut out = Map::new();
    let mut attributes = Vec::new();
    let mut reader = Reader::new(bytes);
    while !reader.done() {
        let (field, wire_type) = reader.tag()?;
        match (field, wire_type) {
            (1, 2) => {
                out.insert("traceId".into(), Value::String(hex(reader.bytes_field()?)));
            }
            (2, 2) => {
                out.insert("spanId".into(), Value::String(hex(reader.bytes_field()?)));
            }
            (4, 2) => attributes.push(decode_key_value(reader.bytes_field()?, 0)?),
            _ => reader.skip(wire_type)?,
        }
    }
    out.insert("attributes".into(), Value::Array(attributes));
    Ok(Value::Object(out))
}

fn decode_status(bytes: &[u8]) -> Decoded<Value> {
    let mut out = Map::new();
    let mut reader = Reader::new(bytes);
    while !reader.done() {
        let (field, wire_type) = reader.tag()?;
        match (field, wire_type) {
            (2, 2) => {
                out.insert(
                    "message".into(),
                    Value::String(utf8(reader.bytes_field()?, "status message")?),
                );
            }
            (3, 0) => {
                out.insert("code".into(), Value::from(reader.varint()?));
            }
            _ => reader.skip(wire_type)?,
        }
    }
    Ok(Value::Object(out))
}

fn decode_key_value(bytes: &[u8], depth: u32) -> Decoded<Value> {
    let mut key = String::new();
    let mut value = Value::Null;
    let mut reader = Reader::new(bytes);
    while !reader.done() {
        let (field, wire_type) = reader.tag()?;
        match (field, wire_type) {
            (1, 2) => key = utf8(reader.bytes_field()?, "attribute key")?,
            (2, 2) => value = decode_any_value(reader.bytes_field()?, depth)?,
            _ => reader.skip(wire_type)?,
        }
    }
    Ok(json!({ "key": key, "value": value }))
}

fn decode_any_value(bytes: &[u8], depth: u32) -> Decoded<Value> {
    if depth > MAX_VALUE_DEPTH {
        return Err(err("attribute value nesting too deep"));
    }
    let mut reader = Reader::new(bytes);
    let mut out = Map::new();
    while !reader.done() {
        let (field, wire_type) = reader.tag()?;
        match (field, wire_type) {
            (1, 2) => {
                out.insert(
                    "stringValue".into(),
                    Value::String(utf8(reader.bytes_field()?, "string value")?),
                );
            }
            (2, 0) => {
                out.insert("boolValue".into(), Value::Bool(reader.varint()? != 0));
            }
            (3, 0) => {
                out.insert("intValue".into(), Value::from(reader.varint()? as i64));
            }
            (4, 1) => {
                let double = f64::from_bits(reader.fixed64()?);
                out.insert(
                    "doubleValue".into(),
                    serde_json::Number::from_f64(double)
                        .map(Value::Number)
                        .unwrap_or(Value::Null),
                );
            }
            (5, 2) => {
                // ArrayValue { repeated AnyValue values = 1; }
                let mut values = Vec::new();
                let mut inner = Reader::new(reader.bytes_field()?);
                while !inner.done() {
                    let (inner_field, inner_type) = inner.tag()?;
                    match (inner_field, inner_type) {
                        (1, 2) => values.push(decode_any_value(inner.bytes_field()?, depth + 1)?),
                        _ => inner.skip(inner_type)?,
                    }
                }
                out.insert("arrayValue".into(), json!({ "values": values }));
            }
            (6, 2) => {
                // KeyValueList { repeated KeyValue values = 1; }
                let mut values = Vec::new();
                let mut inner = Reader::new(reader.bytes_field()?);
                while !inner.done() {
                    let (inner_field, inner_type) = inner.tag()?;
                    match (inner_field, inner_type) {
                        (1, 2) => values.push(decode_key_value(inner.bytes_field()?, depth + 1)?),
                        _ => inner.skip(inner_type)?,
                    }
                }
                out.insert("kvlistValue".into(), json!({ "values": values }));
            }
            (7, 2) => {
                out.insert(
                    "bytesValue".into(),
                    Value::String(hex(reader.bytes_field()?)),
                );
            }
            _ => reader.skip(wire_type)?,
        }
    }
    Ok(Value::Object(out))
}
