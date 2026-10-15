//! gzip request bodies on the ingest routes, process-level.
//!
//! `Content-Encoding: gzip` is what an OpenTelemetry exporter sends once
//! compression is on. Both ingest routes decode it; nothing else accepts it;
//! a body that decompresses past the 64 MiB body cap is refused with `413`
//! before it is materialized; and a damaged body is a `400` naming the damage.
//! The inflate itself is unit-tested in `src/gzip.rs`; this proves the wiring.

mod common;

use common::Server;
use serde_json::{json, Value};

/// Python's `gzip.compress(body, compresslevel=9, mtime=0)` of a three-span
/// native batch (trace `gz-trace`, spans `s0`..`s2`): dynamic Huffman, as
/// real exporters produce.
const BATCH_GZIP: &str = "1f8b0800000000000213ed91cb8a023110457f25d4ba0d9d4610fa376629d294dda506f2b2522dcc88ff6ee20c0c0c822e5cccc2ac52b70ec985b33e83308e34d8097ad87f2d6e1334901386ef30b7650ce8a9dc9df37a44e72a407cb2630d714f416a22c83288f534840cbd59b57f4f0314a67b84f9255084ed76162ac019cad3035acd749c298bf67122578b26592c63f9f3679f38fa24bad5630c52dbf4f0317b8f6c332939903acea51bb1fb544c29b2a85de4db621b9127adfe0f0d974bf38414f32a29e6a114f396f2a494ee5552ba8752bab79422657305a5115382c0040000";

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).expect("hex"))
        .collect()
}

/// CRC-32 (IEEE), bitwise: independent of the server's table-driven one.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffff_u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

/// `plain` as a gzip member of stored (uncompressed) deflate blocks.
fn gzip_stored(plain: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255];
    let mut pieces: Vec<&[u8]> = plain.chunks(65_535).collect();
    if pieces.is_empty() {
        pieces.push(&[]);
    }
    for (index, piece) in pieces.iter().enumerate() {
        out.push(u8::from(index + 1 == pieces.len()));
        out.extend_from_slice(&(piece.len() as u16).to_le_bytes());
        out.extend_from_slice(&(!(piece.len() as u16)).to_le_bytes());
        out.extend_from_slice(piece);
    }
    out.extend_from_slice(&crc32(plain).to_le_bytes());
    out.extend_from_slice(&(plain.len() as u32).to_le_bytes());
    out
}

/// A decompression bomb: one literal and then `matches` copies of 258 bytes
/// at distance 1, in fixed-Huffman codes — 13 bits per 258 output bytes. The
/// trailer is a deliberate dummy: a server that honours its cap refuses the
/// body before it ever reads that far.
fn gzip_bomb(matches: usize) -> Vec<u8> {
    let mut bytes = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255];
    let mut buffer = 0_u64;
    let mut count = 0_u32;
    let mut put = |value: u32, n: u32, bytes: &mut Vec<u8>| {
        buffer |= u64::from(value) << count;
        count += n;
        while count >= 8 {
            bytes.push(buffer as u8);
            buffer >>= 8;
            count -= 8;
        }
    };
    let code = |value: u32, n: u32| value.reverse_bits() >> (32 - n);
    put(1, 1, &mut bytes); // final block
    put(1, 2, &mut bytes); // fixed Huffman
    put(code(0x30 + u32::from(b'x'), 8), 8, &mut bytes); // literal 'x'
    for _ in 0..matches {
        put(code(0b1100_0101, 8), 8, &mut bytes); // length 258
        put(0, 5, &mut bytes); // distance 1
    }
    put(0, 7, &mut bytes); // end of block
    put(0, 7, &mut bytes); // flush the partial byte
    bytes.extend_from_slice(&[0; 8]);
    bytes
}

fn post(server: &Server, path: &str, body: &[u8], extra: &[(&str, &str)]) -> (u16, Value) {
    let mut headers = vec![("Content-Encoding", "gzip")];
    headers.extend_from_slice(extra);
    let (status, _, text) = server.request("POST", path, Some(body), &headers);
    let value = serde_json::from_str(&text).unwrap_or(Value::Null);
    (status, value)
}

#[test]
fn a_compressed_native_batch_is_decoded_and_stored() {
    let server = Server::spawn(&[]);
    let (status, body) = post(&server, "/v1/spans", &unhex(BATCH_GZIP), &[]);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["accepted"], json!(3));
    let (status, trace) = server.json("GET", "/v1/traces/gz-trace", None, &[]);
    assert_eq!(status, 200);
    assert_eq!(trace["spans"].as_array().map(Vec::len), Some(3));
    assert_eq!(
        trace["spans"][0]["attributes"]["gen_ai.request.model"],
        json!("gpt-4o")
    );
    // `x-gzip` is the historical spelling of the same coding.
    let (status, _, _) = server.request(
        "POST",
        "/v1/spans",
        Some(&unhex(BATCH_GZIP)),
        &[("Content-Encoding", "x-gzip")],
    );
    assert_eq!(status, 200);
    // Usage charges the decompressed spans, not the wire bytes.
    let (_, usage) = server.json("GET", "/v1/usage?tenant=", None, &[]);
    assert!(
        usage["tenants"][0]["lifetime"]["bytes"]
            .as_u64()
            .unwrap_or(0)
            > 1_000
    );
}

#[test]
fn compressed_otlp_json_and_protobuf_are_decoded() {
    let server = Server::spawn(&[]);
    let otlp = json!({"resourceSpans": [{
        "resource": {"attributes": [{"key": "service.name", "value": {"stringValue": "svc"}}]},
        "scopeSpans": [{"spans": [{
            "traceId": "0102030405060708090a0b0c0d0e0f10", "spanId": "0102030405060708",
            "name": "otlp-gz", "startTimeUnixNano": "1000", "endTimeUnixNano": "2000"}]}]}]});
    let encoded = serde_json::to_vec(&otlp).expect("encodes");
    let (status, body) = post(&server, "/v1/traces", &gzip_stored(&encoded), &[]);
    assert_eq!((status, body), (200, json!({"partialSuccess": {}})));

    // Protobuf: one span, hand-encoded (trace 1, span 2, name 5, times 7/8).
    let mut span = vec![0x0a, 16];
    span.extend_from_slice(&[0xcd; 16]);
    span.extend_from_slice(&[0x12, 8]);
    span.extend_from_slice(&[0x02; 8]);
    span.extend_from_slice(&[0x2a, 6]);
    span.extend_from_slice(b"pb-gz!");
    span.push(0x39);
    span.extend_from_slice(&1_000_u64.to_le_bytes());
    span.push(0x41);
    span.extend_from_slice(&2_000_u64.to_le_bytes());
    let scope = [&[0x12, span.len() as u8][..], &span].concat();
    let resource = [&[0x12, scope.len() as u8][..], &scope].concat();
    let request = [&[0x0a, resource.len() as u8][..], &resource].concat();
    let (status, _, _) = server.request(
        "POST",
        "/v1/traces",
        Some(&gzip_stored(&request)),
        &[
            ("Content-Encoding", "gzip"),
            ("Content-Type", "application/x-protobuf"),
        ],
    );
    assert_eq!(status, 200);
    let (_, found) = server.json("GET", "/v1/spans?name=pb-gz!", None, &[]);
    assert_eq!(found["spans"].as_array().map(Vec::len), Some(1), "{found}");
    let (_, found) = server.json("GET", "/v1/spans?name=otlp-gz", None, &[]);
    assert_eq!(found["spans"].as_array().map(Vec::len), Some(1), "{found}");
}

#[test]
fn other_codings_and_other_routes_are_refused_with_415() {
    let server = Server::spawn(&[]);
    let body = gzip_stored(b"[]");
    let (status, refusal) = server.json("POST", "/v1/spans", None, &[("Content-Encoding", "br")]);
    assert_eq!(status, 415);
    assert_eq!(
        refusal["error"],
        json!("unsupported Content-Encoding: br; send gzip or an uncompressed body")
    );
    let (status, refusal) = post(&server, "/v1/annotations", &body, &[]);
    assert_eq!(status, 415);
    assert!(refusal["error"]
        .as_str()
        .unwrap_or_default()
        .contains("only"));
    // Two gzip headers mean gzip applied twice, which is not decoded once
    // and then misparsed.
    let (status, _, _) = server.request(
        "POST",
        "/v1/spans",
        Some(&body),
        &[("Content-Encoding", "gzip"), ("Content-Encoding", "gzip")],
    );
    assert_eq!(status, 415);
    // `identity` is no coding at all.
    let (status, _, _) = server.request(
        "POST",
        "/v1/spans",
        Some(b"[]"),
        &[("Content-Encoding", "identity")],
    );
    assert_eq!(status, 200);
}

#[test]
fn a_damaged_body_is_a_400_naming_the_damage() {
    let server = Server::spawn(&[]);
    let mut corrupt = unhex(BATCH_GZIP);
    let at = corrupt.len() - 8;
    corrupt[at] ^= 0x01;
    let (status, body) = post(&server, "/v1/spans", &corrupt, &[]);
    assert_eq!(status, 400);
    assert_eq!(body["error"], json!("invalid gzip body: CRC-32 mismatch"));
    let (status, body) = post(&server, "/v1/spans", b"[{\"not\":\"gzip\"}]", &[]);
    assert_eq!(status, 400);
    assert!(body["error"].as_str().unwrap_or_default().contains("magic"));
    let (_, spans) = server.json("GET", "/v1/spans", None, &[]);
    assert_eq!(spans["spans"].as_array().map(Vec::len), Some(0));
}

#[test]
fn a_body_that_inflates_past_the_body_cap_is_refused_with_413() {
    let server = Server::spawn(&[]);
    // 64 MiB / 258 matches, plus margin: past the cap by a few hundred KiB,
    // from a body of about 420 KiB.
    let matches = (64 * 1024 * 1024) / 258 + 1_000;
    let bomb = gzip_bomb(matches);
    assert!(bomb.len() < 512 * 1024, "{}", bomb.len());
    let (status, body) = post(&server, "/v1/spans", &bomb, &[]);
    assert_eq!(status, 413, "{body}");
    assert_eq!(
        body["error"],
        json!(format!(
            "gzip body decompresses to more than {} bytes",
            64 * 1024 * 1024
        ))
    );
    // The server is fine afterwards.
    let (status, _) = server.json("GET", "/healthz", None, &[]);
    assert_eq!(status, 200);
}
