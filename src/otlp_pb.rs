//! OTLP/HTTP binary-protobuf decoding, dependency-free.
//!
//! A hand-rolled protobuf wire decoder for the OTLP `TracesData` /
//! `ExportTraceServiceRequest` message (they share field 1:
//! `repeated ResourceSpans`), lowering straight to [`crate::Span`].
//!
//! It used to lower to the OTLP/HTTP JSON `serde_json::Value` shape and hand
//! that to the JSON mapper — one mapping, two encodings. That was the right
//! call for correctness and the wrong one for cost: it built a `Map` and a
//! `String` key for every span, every `KeyValue` and every `AnyValue` on the
//! wire, and hex-encoded ids through a `format!` per BYTE, only for the mapper
//! to take it all apart again. Protobuf ended up decoding SLOWER than the JSON
//! it was supposed to beat, on a payload a third the size.
//!
//! The mapping is still shared, just at a lower level: this decoder fills in
//! the same [`SpanParts`] and [`AnyValueParts`] the JSON decoder does, so
//! attribute precedence, status text, link filtering and the non-empty id rule
//! are decided in one place for both encodings.
//!
//! Wire-format subset: varint (0), fixed64 (1), length-delimited (2),
//! fixed32 (5). Unknown fields are skipped by wire type, as the format
//! requires. All slicing is bounds-checked; malformed input yields
//! `DecodeError`, never a panic.

use crate::otlp::{hex, AnyValueParts, OtlpError, SpanParts};
use crate::{Event, Link, Span};
use serde_json::{Map, Value};

/// A malformed protobuf payload (truncated varint, bad length, wrong type).
#[derive(Debug)]
pub struct DecodeError(pub String);

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "protobuf decode error: {}", self.0)
    }
}

impl std::error::Error for DecodeError {}

impl From<OtlpError> for DecodeError {
    fn from(error: OtlpError) -> Self {
        DecodeError(error.0)
    }
}

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

// -------------------------------------------------------- message decoders

/// Decodes an `ExportTraceServiceRequest` / `TracesData` payload to spans.
pub fn spans_from_protobuf(bytes: &[u8]) -> Decoded<Vec<Span>> {
    let mut spans = Vec::new();
    let mut reader = Reader::new(bytes);
    while !reader.done() {
        let (field, wire_type) = reader.tag()?;
        match (field, wire_type) {
            (1, 2) => decode_resource_spans(reader.bytes_field()?, &mut spans)?,
            _ => reader.skip(wire_type)?,
        }
    }
    Ok(spans)
}

/// Field order on the wire is not guaranteed, and `resource` (field 1) sets
/// the service every span in `scopeSpans` (field 2) inherits. So the resource
/// is read in a first sweep and the spans in a second, rather than assuming
/// the encoder emitted them in field order. The sweep only re-walks tags —
/// submessages it does not want are skipped, not decoded.
fn decode_resource_spans(bytes: &[u8], out: &mut Vec<Span>) -> Decoded<()> {
    let mut service = None;
    let mut reader = Reader::new(bytes);
    while !reader.done() {
        let (field, wire_type) = reader.tag()?;
        match (field, wire_type) {
            (1, 2) => service = decode_resource_service(reader.bytes_field()?)?,
            _ => reader.skip(wire_type)?,
        }
    }
    let service = service.unwrap_or_else(|| "unknown_service".to_owned());

    let mut reader = Reader::new(bytes);
    while !reader.done() {
        let (field, wire_type) = reader.tag()?;
        match (field, wire_type) {
            (2, 2) => decode_scope_spans(reader.bytes_field()?, &service, out)?,
            _ => reader.skip(wire_type)?,
        }
    }
    Ok(())
}

/// The first `service.name` resource attribute whose value is a string.
///
/// Every resource attribute is still decoded in full, not just the one being
/// looked for: decoding is what rejects invalid UTF-8 and hostile nesting, and
/// skipping the others would quietly start accepting malformed resources. The
/// cost is per-ResourceSpans, not per-span.
fn decode_resource_service(bytes: &[u8]) -> Decoded<Option<String>> {
    let mut reader = Reader::new(bytes);
    let mut service = None;
    while !reader.done() {
        let (field, wire_type) = reader.tag()?;
        match (field, wire_type) {
            (1, 2) => {
                let (key, value) = decode_key_value(reader.bytes_field()?, 0)?;
                if service.is_none() && key == "service.name" {
                    if let Value::String(name) = value {
                        service = Some(name);
                    }
                }
            }
            _ => reader.skip(wire_type)?,
        }
    }
    Ok(service)
}

/// Same two-sweep reason as [`decode_resource_spans`]: `scope` is field 1 and
/// supplies attributes that sit beneath every span in field 2.
fn decode_scope_spans(bytes: &[u8], service: &str, out: &mut Vec<Span>) -> Decoded<()> {
    let mut scope_attributes = Map::new();
    let mut reader = Reader::new(bytes);
    while !reader.done() {
        let (field, wire_type) = reader.tag()?;
        match (field, wire_type) {
            (1, 2) => scope_attributes = decode_scope_attributes(reader.bytes_field()?)?,
            _ => reader.skip(wire_type)?,
        }
    }

    let mut reader = Reader::new(bytes);
    while !reader.done() {
        let (field, wire_type) = reader.tag()?;
        match (field, wire_type) {
            (2, 2) => {
                let parts = decode_span(reader.bytes_field()?)?;
                out.push(parts.finish(service, &scope_attributes)?);
            }
            _ => reader.skip(wire_type)?,
        }
    }
    Ok(())
}

fn decode_scope_attributes(bytes: &[u8]) -> Decoded<Map<String, Value>> {
    let mut attributes = Map::new();
    let mut reader = Reader::new(bytes);
    while !reader.done() {
        let (field, wire_type) = reader.tag()?;
        match (field, wire_type) {
            (3, 2) => {
                let (key, value) = decode_key_value(reader.bytes_field()?, 0)?;
                attributes.insert(key, value);
            }
            _ => reader.skip(wire_type)?,
        }
    }
    Ok(attributes)
}

fn decode_span(bytes: &[u8]) -> Decoded<SpanParts> {
    let mut trace_id = None;
    let mut span_id = None;
    let mut parent_span_id = String::new();
    let mut name = String::new();
    let mut start_time_ns = None;
    let mut end_time_ns = None;
    let mut status_code = None;
    let mut attributes = Map::new();
    let mut events = Vec::new();
    let mut links = Vec::new();
    let mut reader = Reader::new(bytes);
    while !reader.done() {
        let (field, wire_type) = reader.tag()?;
        match (field, wire_type) {
            (1, 2) => trace_id = Some(hex(reader.bytes_field()?)),
            (2, 2) => span_id = Some(hex(reader.bytes_field()?)),
            (4, 2) => parent_span_id = hex(reader.bytes_field()?),
            (5, 2) => name = utf8(reader.bytes_field()?, "span name")?,
            (7, 1) => start_time_ns = Some(reader.fixed64()?),
            (8, 1) => end_time_ns = Some(reader.fixed64()?),
            (9, 2) => {
                let (key, value) = decode_key_value(reader.bytes_field()?, 0)?;
                attributes.insert(key, value);
            }
            (11, 2) => events.push(decode_event(reader.bytes_field()?)?),
            (13, 2) => links.push(decode_link(reader.bytes_field()?)?),
            (15, 2) => status_code = decode_status_code(reader.bytes_field()?)?,
            _ => reader.skip(wire_type)?,
        }
    }
    Ok(SpanParts {
        trace_id: trace_id.ok_or_else(|| err("traceId is required"))?,
        span_id: span_id.ok_or_else(|| err("spanId is required"))?,
        parent_span_id,
        name,
        start_time_ns: start_time_ns.ok_or_else(|| err("startTimeUnixNano is required"))?,
        end_time_ns: end_time_ns.ok_or_else(|| err("endTimeUnixNano is required"))?,
        status_code,
        attributes,
        events,
        links,
    })
}

fn decode_event(bytes: &[u8]) -> Decoded<Event> {
    let mut timestamp_ns = None;
    let mut name = String::new();
    let mut attributes = Map::new();
    let mut reader = Reader::new(bytes);
    while !reader.done() {
        let (field, wire_type) = reader.tag()?;
        match (field, wire_type) {
            (1, 1) => timestamp_ns = Some(reader.fixed64()?),
            (2, 2) => name = utf8(reader.bytes_field()?, "event name")?,
            (3, 2) => {
                let (key, value) = decode_key_value(reader.bytes_field()?, 0)?;
                attributes.insert(key, value);
            }
            _ => reader.skip(wire_type)?,
        }
    }
    Ok(Event {
        name,
        timestamp_ns: timestamp_ns.ok_or_else(|| err("event timeUnixNano is required"))?,
        attributes,
    })
}

fn decode_link(bytes: &[u8]) -> Decoded<Link> {
    let mut trace_id = None;
    let mut span_id = None;
    let mut attributes = Map::new();
    let mut reader = Reader::new(bytes);
    while !reader.done() {
        let (field, wire_type) = reader.tag()?;
        match (field, wire_type) {
            (1, 2) => trace_id = Some(hex(reader.bytes_field()?)),
            (2, 2) => span_id = Some(hex(reader.bytes_field()?)),
            (4, 2) => {
                let (key, value) = decode_key_value(reader.bytes_field()?, 0)?;
                attributes.insert(key, value);
            }
            _ => reader.skip(wire_type)?,
        }
    }
    Ok(Link {
        trace_id: trace_id.ok_or_else(|| err("link traceId is required"))?,
        span_id: span_id.ok_or_else(|| err("link spanId is required"))?,
        attributes,
    })
}

/// `Status { string message = 2; StatusCode code = 3; }`. The message is not
/// mapped onto the span model, so only the code is read.
fn decode_status_code(bytes: &[u8]) -> Decoded<Option<u64>> {
    let mut code = None;
    let mut reader = Reader::new(bytes);
    while !reader.done() {
        let (field, wire_type) = reader.tag()?;
        match (field, wire_type) {
            (3, 0) => code = Some(reader.varint()?),
            _ => reader.skip(wire_type)?,
        }
    }
    Ok(code)
}

/// A `KeyValue`, flattened. A missing key reads as the empty string rather
/// than skipping the entry, which is what the old lowering did by synthesising
/// `{"key": ""}`; the schema makes key required, so only malformed input sees it.
fn decode_key_value(bytes: &[u8], depth: u32) -> Decoded<(String, Value)> {
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
    Ok((key, value))
}

fn decode_any_value(bytes: &[u8], depth: u32) -> Decoded<Value> {
    if depth > MAX_VALUE_DEPTH {
        return Err(err("attribute value nesting too deep"));
    }
    let mut reader = Reader::new(bytes);
    let mut parts = AnyValueParts::default();
    while !reader.done() {
        let (field, wire_type) = reader.tag()?;
        match (field, wire_type) {
            (1, 2) => {
                parts.string = Some(Value::String(utf8(reader.bytes_field()?, "string value")?));
            }
            (2, 0) => parts.boolean = Some(Value::Bool(reader.varint()? != 0)),
            (3, 0) => parts.int = Some(Value::from(reader.varint()? as i64)),
            (4, 1) => {
                let double = f64::from_bits(reader.fixed64()?);
                // A non-finite double has no JSON number; it reads as null,
                // which is what the value-shaped lowering produced too.
                parts.double = Some(
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
                parts.array = Some(values);
            }
            (6, 2) => {
                // KeyValueList { repeated KeyValue values = 1; }
                let mut values = Map::new();
                let mut inner = Reader::new(reader.bytes_field()?);
                while !inner.done() {
                    let (inner_field, inner_type) = inner.tag()?;
                    match (inner_field, inner_type) {
                        (1, 2) => {
                            let (key, value) = decode_key_value(inner.bytes_field()?, depth + 1)?;
                            values.insert(key, value);
                        }
                        _ => inner.skip(inner_type)?,
                    }
                }
                parts.kvlist = Some(values);
            }
            // BytesValue. Copied out because `AnyValueParts` owns what it
            // carries; only an attribute that actually is bytes pays for it.
            (7, 2) => parts.bytes = Some(reader.bytes_field()?.to_vec()),
            _ => reader.skip(wire_type)?,
        }
    }
    Ok(parts.resolve())
}
