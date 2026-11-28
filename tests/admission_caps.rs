//! Admission caps, process-level.
//!
//! Each cap refuses a violating batch whole, with a `400` naming the cap and
//! the span, on both ingest surfaces; the defaults apply with no flags; `0`
//! disables a cap; `--max-span-bytes` measures the span as STORED, after
//! payload offload; and a cap refusal is the client's defect, not a quota
//! rejection, so it never touches the tenant's usage counters.

mod common;

use common::Server;
use serde_json::{json, Map, Value};

fn span(id: &str, attributes: Value) -> Value {
    json!({"trace_id": "t-caps", "span_id": id, "name": "op", "service": "svc",
           "start_time_ns": 1_000u64, "end_time_ns": 2_000u64, "attributes": attributes})
}

fn wide(count: usize) -> Value {
    let mut attributes = Map::new();
    for index in 0..count {
        attributes.insert(format!("k{index}"), json!(index));
    }
    Value::Object(attributes)
}

fn refused(server: &Server, batch: &Value) -> String {
    let (status, body) = server.json("POST", "/v1/spans", Some(batch), &[]);
    assert_eq!(status, 400, "{body}");
    body["error"].as_str().expect("error").to_owned()
}

fn stored(server: &Server) -> usize {
    let (status, body) = server.json("GET", "/v1/spans?limit=1000", None, &[]);
    assert_eq!(status, 200);
    body["spans"].as_array().map_or(0, Vec::len)
}

#[test]
fn each_cap_refuses_the_whole_batch_naming_itself_and_the_span() {
    let server = Server::spawn(&[
        "--max-span-attributes",
        "3",
        "--max-attribute-key-bytes",
        "10",
        "--max-span-events",
        "1",
        "--max-span-bytes",
        "2000",
        "--max-batch-spans",
        "4",
    ]);
    let good = span("ok", json!({"a": 1}));

    let message = refused(&server, &json!([good, good, good, good, good]));
    assert_eq!(
        message,
        "batch of 5 spans exceeds the max-batch-spans cap of 4"
    );

    let message = refused(&server, &json!([good, span("s-wide", wide(4))]));
    assert_eq!(
        message,
        "span 1 (trace t-caps, span s-wide): 4 attributes exceeds the max-span-attributes cap of 3"
    );

    let message = refused(&server, &json!([span("s-key", json!({"an-eleven-b": 1}))]));
    assert!(
        message.contains("span 0 (trace t-caps, span s-key)")
            && message.contains("11 bytes exceeds the max-attribute-key-bytes cap of 10"),
        "{message}"
    );
    // Keys inside events count too.
    let mut evented = span("s-event-key", json!({}));
    evented["events"] = json!([{"name": "e", "timestamp_ns": 1, "attributes": {"an-eleven-b": 1}}]);
    assert!(refused(&server, &json!([evented])).contains("max-attribute-key-bytes"));

    let mut chatty = span("s-events", json!({}));
    chatty["events"] = json!([{"name": "a", "timestamp_ns": 1}, {"name": "b", "timestamp_ns": 2}]);
    assert!(refused(&server, &json!([chatty]))
        .contains("2 events exceeds the max-span-events cap of 1"));

    let message = refused(
        &server,
        &json!([span("s-big", json!({"a": "x".repeat(3000)}))]),
    );
    assert!(
        message.contains("after payload offload exceeds the max-span-bytes cap of 2000"),
        "{message}"
    );

    // Batch-atomic: none of the good spans riding with a bad one landed.
    assert_eq!(stored(&server), 0);
    let (status, _) = server.json("POST", "/v1/spans", Some(&json!([good])), &[]);
    assert_eq!(status, 200);
    assert_eq!(stored(&server), 1);

    // OTLP is held to the same caps.
    let attributes: Vec<Value> = (0..4)
        .map(|index| json!({"key": format!("k{index}"), "value": {"intValue": index}}))
        .collect();
    let otlp = json!({"resourceSpans": [{
        "resource": {"attributes": [{"key": "service.name", "value": {"stringValue": "svc"}}]},
        "scopeSpans": [{"spans": [{
            "traceId": "0102030405060708090a0b0c0d0e0f10", "spanId": "0102030405060708",
            "name": "op", "startTimeUnixNano": "1000", "endTimeUnixNano": "2000",
            "attributes": attributes}]}]}]});
    let (status, body) = server.json("POST", "/v1/traces", Some(&otlp), &[]);
    assert_eq!(status, 400, "{body}");
    assert!(body["error"]
        .as_str()
        .unwrap_or_default()
        .contains("max-span-attributes"));

    // None of that was a quota rejection.
    let (_, usage) = server.json("GET", "/v1/usage?tenant=", None, &[]);
    assert_eq!(usage["tenants"][0]["lifetime"]["rejected"], json!(0));
    assert_eq!(usage["tenants"][0]["lifetime"]["spans"], json!(1));
}

#[test]
fn the_defaults_apply_with_no_flags() {
    let server = Server::spawn(&[]);
    let message = refused(&server, &json!([span("s", wide(1025))]));
    assert!(
        message.contains("max-span-attributes cap of 1024"),
        "{message}"
    );
    let long_key = "k".repeat(257);
    let message = refused(&server, &json!([span("s", json!({long_key: 1}))]));
    assert!(
        message.contains("max-attribute-key-bytes cap of 256"),
        "{message}"
    );
    let (status, _) = server.json(
        "POST",
        "/v1/spans",
        Some(&json!([span("s", wide(1024))])),
        &[],
    );
    assert_eq!(status, 200, "exactly at the default cap is allowed");
}

#[test]
fn zero_disables_a_cap() {
    let server = Server::spawn(&["--max-span-attributes", "0", "--max-batch-spans", "0"]);
    let (status, body) = server.json(
        "POST",
        "/v1/spans",
        Some(&json!([span("s", wide(2000))])),
        &[],
    );
    assert_eq!(status, 200, "{body}");
}

#[test]
fn the_span_byte_cap_measures_the_span_as_stored_after_offload() {
    let server = Server::spawn(&[
        "--payload-threshold-bytes",
        "1024",
        "--max-span-bytes",
        "4096",
    ]);
    // One 100 KB prompt: offloaded to a payload file, so the stored span is
    // small and admitted.
    let (status, body) = server.json(
        "POST",
        "/v1/spans",
        Some(&json!([span(
            "prompt",
            json!({"llm.prompt": "p".repeat(100_000)})
        )])),
        &[],
    );
    assert_eq!(status, 200, "{body}");
    // Ten 900-byte values: each under the offload threshold, so all of it
    // stays inline and the stored span is over the cap.
    let mut inline = Map::new();
    for index in 0..10 {
        inline.insert(format!("v{index}"), json!("i".repeat(900)));
    }
    let message = refused(
        &server,
        &json!([span("inline", Value::Object(inline.clone()))]),
    );
    assert!(message.contains("max-span-bytes cap of 4096"), "{message}");

    // The cap is decided BEFORE the offload writes anything: a refused span
    // whose prompt would have been offloaded leaves no payload file behind.
    assert_eq!(payload_files(&server), 1, "the admitted prompt's");
    inline.insert("llm.prompt".into(), json!("q".repeat(100_000)));
    refused(&server, &json!([span("both", Value::Object(inline))]));
    assert_eq!(
        payload_files(&server),
        1,
        "a refused batch wrote a payload file"
    );
}

fn payload_files(server: &Server) -> usize {
    let mut count = 0;
    let mut pending = vec![server.data_dir.join("payloads")];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.map_while(Result::ok) {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                pending.push(entry.path());
            } else if entry.path().extension().is_some_and(|ext| ext == "bin") {
                count += 1;
            }
        }
    }
    count
}

#[test]
fn a_bad_cap_value_refuses_startup() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_traza-server"))
        .args(["--port", "0", "--max-span-bytes", "lots"])
        .env_remove("TRAZA_TOKENS")
        .output()
        .expect("runs");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("--max-span-bytes must be a non-negative number"));
}
