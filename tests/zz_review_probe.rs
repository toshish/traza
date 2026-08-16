//! Temporary review probes. Delete.

use serde_json::{json, Value};
use traza::mcp::{Access, Context, Limits, Server as McpServer};
use traza::{Config, Span, Store};

fn tmp(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "traza-probe-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

#[allow(clippy::too_many_arguments)]
fn span(
    tenant: &str,
    trace: &str,
    id: &str,
    parent: Option<&str>,
    name: &str,
    status: &str,
    start: u64,
    end: u64,
    session: &str,
    extra: Value,
) -> Span {
    let mut attributes = json!({"session.id": session});
    if let (Some(dst), Some(src)) = (attributes.as_object_mut(), extra.as_object()) {
        for (k, v) in src {
            dst.insert(k.clone(), v.clone());
        }
    }
    serde_json::from_value(json!({
        "trace_id": trace, "span_id": id, "parent_span_id": parent,
        "$tenant": tenant,
        "name": name, "service": "agent", "status": status,
        "start_time_ns": start, "end_time_ns": end,
        "attributes": attributes,
    }))
    .expect("span")
}

const IDLE: u64 = 900_000_000_000;

fn call(store: &Store, tenant: Option<&str>, now: u64, name: &str, arguments: Value) -> Value {
    let server = McpServer::new(store, Limits::default(), true).with_promotion(true);
    let response = server
        .handle(
            &json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": name, "arguments": arguments},
            }),
            Context {
                access: Access::ReadWrite,
                tenant: tenant.map(str::to_owned),
                now_ns: now,
            },
        )
        .expect("response");
    response["result"].clone()
}

/// PROBE 1: an unbound caller's promotion lands in a FOREIGN tenant's dataset,
/// because the by-name lookup runs unscoped.
#[test]
fn probe_unbound_promote_lands_in_foreign_tenant_dataset() {
    let dir = tmp("crosstenant");
    let store = Store::open(&dir, Config::default()).expect("opens");

    let base = 1_700_000_000_000_000_000_u64;
    // Tenant "beta" runs a session with a failing leaf.
    store
        .ingest(span(
            "beta",
            "aa".repeat(16).as_str(),
            "b1".repeat(8).as_str(),
            None,
            "root",
            "error",
            base,
            base + 1_000,
            "s-1",
            json!({}),
        ))
        .expect("ingest");
    store
        .ingest(span(
            "beta",
            "aa".repeat(16).as_str(),
            "b2".repeat(8).as_str(),
            Some("b1".repeat(8).as_str()),
            "vector.search",
            "error",
            base + 10,
            base + 500,
            "s-1",
            json!({"prompt": "BETA-CONFIDENTIAL-PROMPT-TEXT"}),
        ))
        .expect("ingest");
    store.flush().expect("flush");

    // Tenant "acme" already owns a dataset called "regressions".
    let acme_dataset = store
        .create_dataset("acme", "regressions")
        .expect("dataset");

    let now = base + 10 * IDLE;
    let promoted = call(
        &store,
        None, // unbound operator credential
        now,
        "promote_failures_to_dataset",
        json!({"session_id": "s-1", "dataset": "regressions"}),
    );
    println!("promote result: {promoted}");
    assert_ne!(promoted["isError"], json!(true), "promotion failed");

    let landed = promoted["structuredContent"]["dataset_id"]
        .as_u64()
        .expect("dataset id");
    println!("acme_dataset={acme_dataset} landed={landed}");
    assert_eq!(
        landed, acme_dataset,
        "the unbound promotion landed in acme's dataset"
    );

    // And a BOUND acme credential can now read beta's span content.
    let version_id = promoted["structuredContent"]["version_id"]
        .as_str()
        .expect("version id");
    let view = store
        .dataset_version(Some("acme"), landed, version_id)
        .expect("read")
        .expect("exists")
        .expect("not tombstoned");
    let rendered = serde_json::to_string(&view.bodies).expect("json");
    println!("acme sees: {rendered}");
    assert!(
        rendered.contains("BETA-CONFIDENTIAL-PROMPT-TEXT"),
        "acme reads beta's content out of its own dataset"
    );
    assert!(
        rendered.contains("\"tenant\":\"beta\""),
        "and the provenance says beta"
    );

    // Now beta erases the trace. The receipt scans only beta-owned versions.
    let status = store
        .erase(traza::erasure::Subject::Trace {
            trace_id: "aa".repeat(16),
            tenant: "beta".to_owned(),
        })
        .expect("erase");
    let receipt = store.verify_erasure(status.erase.id).expect("verify");
    println!(
        "receipt: {}",
        serde_json::to_string_pretty(&receipt).unwrap()
    );
    let still_there = store
        .dataset_version(Some("acme"), landed, version_id)
        .expect("read")
        .expect("exists")
        .expect("not tombstoned");
    println!(
        "after erasure acme still sees: {}",
        serde_json::to_string(&still_there.bodies).unwrap()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// PROBE 2: diagnose_session's structuredContent is not budgeted, so the whole
/// result blows past --mcp-max-result-bytes.
#[test]
fn probe_diagnosis_result_exceeds_the_ceiling() {
    let dir = tmp("ceiling");
    let store = Store::open(&dir, Config::default()).expect("opens");
    let base = 1_700_000_000_000_000_000_u64;

    // 300 distinct operations, each repeated 5 times inside one trace.
    let trace = "cc".repeat(16);
    let mut n = 0_u64;
    for group in 0..300 {
        for attempt in 0..5 {
            n += 1;
            let sid = format!("{n:016x}");
            store
                .ingest(span(
                    "",
                    &trace,
                    &sid,
                    None,
                    &format!("tool.call.number.{group}"),
                    "ok",
                    base + n * 10,
                    base + n * 10 + 5,
                    "big",
                    json!({"attempt": attempt}),
                ))
                .expect("ingest");
        }
    }
    store.flush().expect("flush");

    let now = base + 10 * IDLE;
    let server = McpServer::new(&store, Limits::default(), true);
    let response = server
        .handle(
            &json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "diagnose_session", "arguments": {"session_id": "big"}},
            }),
            Context {
                access: Access::Read,
                tenant: None,
                now_ns: now,
            },
        )
        .expect("response");
    let result = &response["result"];
    let bytes = serde_json::to_vec(result).expect("json").len();
    let text_len = result["content"][0]["text"].as_str().unwrap_or("").len();
    let findings = result["structuredContent"]["findings"]
        .as_array()
        .map(Vec::len)
        .unwrap_or(0);
    println!(
        "ceiling={} result={} bytes, text={} bytes, findings={}",
        Limits::default().max_result_bytes,
        bytes,
        text_len,
        findings
    );
    assert!(
        bytes <= Limits::default().max_result_bytes,
        "result is {bytes} bytes, ceiling is {}",
        Limits::default().max_result_bytes
    );

    let _ = std::fs::remove_dir_all(&dir);
}
