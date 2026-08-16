//! Temporary review probe. Delete after the review.
use serde_json::{json, Value};
use traza::mcp::{Access, Context, Limits, Server as McpServer};
use traza::{Config, Span, Store};

fn tmp(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "traza-review-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

fn span(tenant: &str, session: &str, id: &str, parent: Option<&str>, status: &str) -> Span {
    serde_json::from_value(json!({
        "$tenant": tenant,
        "trace_id": "trace-1",
        "span_id": id,
        "parent_span_id": parent,
        "name": if parent.is_some() { "tool" } else { "workflow" },
        "service": "agent",
        "status": status,
        "start_time_ns": 1_000_000_000_u64 + id.len() as u64,
        "end_time_ns": 2_000_000_000_u64,
        "attributes": {"session.id": session, "secret": "beta-private-prompt-text"},
    }))
    .expect("span")
}

#[test]
fn unbound_promote_lands_in_a_foreign_tenants_dataset() {
    let dir = tmp("xtenant");
    let store = Store::open(&dir, Config::default()).expect("opens");

    // acme already owns a dataset called "regressions".
    let acme_dataset = store
        .create_dataset("acme", "regressions")
        .expect("creates");

    // beta has a failing session.
    store
        .ingest(span("beta", "beta-session", "root", None, "error"))
        .expect("ingests");
    store
        .ingest(span("beta", "beta-session", "leaf", Some("root"), "error"))
        .expect("ingests");
    store.flush().expect("seals");

    let server = McpServer::new(&store, Limits::default(), true).with_promotion(true);
    let response = server
        .handle(
            &json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "promote_failures_to_dataset",
                           "arguments": {"session_id": "beta-session",
                                         "dataset": "regressions"}},
            }),
            // UNBOUND operator credential: no tenant binding.
            Context {
                access: Access::ReadWrite,
                tenant: None,
                now_ns: 900_000_000_000_000,
            },
        )
        .expect("a response");
    println!("PROMOTE RESPONSE: {response}");
    let landed = response["result"]["structuredContent"]["dataset_id"].as_u64();
    println!("acme dataset_id = {acme_dataset}, landed in = {landed:?}");

    let views = store.datasets(None).expect("datasets");
    for view in &views {
        println!(
            "dataset {} tenant={:?} name={:?} versions={}",
            view.dataset.dataset_id,
            view.dataset.tenant,
            view.dataset.name,
            view.versions.len()
        );
    }
    assert_eq!(
        landed,
        Some(acme_dataset),
        "beta's failing spans were written into acme's dataset"
    );

    // And the content is readable by an acme-bound principal.
    let version_id = response["result"]["structuredContent"]["version_id"]
        .as_str()
        .expect("version id")
        .to_owned();
    let stored = store
        .dataset_version(Some("acme"), acme_dataset, &version_id)
        .expect("reads")
        .expect("exists")
        .expect("not tombstoned");
    let rendered: Value = serde_json::to_value(&stored.bodies).expect("json");
    println!("ACME-VISIBLE BODIES: {rendered}");
    assert!(
        rendered.to_string().contains("beta-private-prompt-text"),
        "acme can read beta's span content"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
