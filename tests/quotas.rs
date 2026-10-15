//! Tenant limits, usage counters and quota enforcement, process-level, plus
//! the runtime retention override at the store level (the server's expiry
//! pass runs once a minute, which no test should wait for).
//!
//! The contract: an unbound admin replaces a tenant's limits document at
//! runtime; every acknowledged batch is counted per tenant — spans, bytes
//! measured BEFORE payload offload, batches — and a batch that would take
//! the month past a limit, or that belongs to a paused tenant, is refused
//! whole with `429`, `Retry-After`, and a body naming the limit. Reads,
//! exports and erasure are never blocked. Counters and limits survive a
//! crash (the counters up to their last write).

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use common::{header, Server};
use serde_json::{json, Value};
use traza::limits::{RetentionSource, TenantLimits};
use traza::{Config, Span, Store};

const TOKENS: &str = "admin:root,rw@acme:acme-rw,ro@acme:acme-ro,rw@bigco:bigco-rw";

fn call(
    server: &Server,
    token: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> (u16, Value) {
    let authorization = format!("Bearer {token}");
    server.json(
        method,
        path,
        body,
        &[("Authorization", authorization.as_str())],
    )
}

fn spans(trace: &str, count: usize) -> Value {
    Value::Array(
        (0..count)
            .map(|index| {
                json!({"trace_id": trace, "span_id": format!("s{index}"), "name": "op",
                       "service": "svc", "start_time_ns": 1_000u64, "end_time_ns": 2_000u64})
            })
            .collect(),
    )
}

fn set_limits(server: &Server, tenant: &str, limits: Value) -> Value {
    let (status, body) = call(
        server,
        "root",
        "POST",
        &format!("/v1/admin/tenants/{tenant}/limits"),
        Some(&limits),
    );
    assert_eq!(status, 200, "{body}");
    body
}

fn usage_row(server: &Server, tenant: &str) -> Value {
    let (status, body) = call(
        server,
        "root",
        "GET",
        &format!("/v1/usage?tenant={tenant}"),
        None,
    );
    assert_eq!(status, 200, "{body}");
    let rows = body["tenants"].as_array().expect("rows");
    assert_eq!(rows.len(), 1, "{body}");
    rows[0].clone()
}

#[test]
fn limits_documents_are_replaced_whole_and_survive_a_crash() {
    let mut server = Server::spawn_with_env(&[], &[("TRAZA_TOKENS", TOKENS)]);
    let (status, defaults) = call(
        &server,
        "root",
        "GET",
        "/v1/admin/tenants/acme/limits",
        None,
    );
    assert_eq!(status, 200);
    assert_eq!(
        defaults,
        json!({"tenant": "acme", "monthly_spans": null, "monthly_bytes": null,
               "ingest_paused": false, "ttl_seconds": null}),
        "a tenant never configured reads as no limits"
    );

    let stored = set_limits(
        &server,
        "acme",
        json!({"monthly_spans": 1000, "monthly_bytes": 5000000, "ingest_paused": false,
               "ttl_seconds": 86400}),
    );
    assert_eq!(
        stored,
        json!({"tenant": "acme", "monthly_spans": 1000, "monthly_bytes": 5000000,
               "ingest_paused": false, "ttl_seconds": 86400})
    );
    // Full replace: what is not sent is reset, not merged.
    let replaced = set_limits(&server, "acme", json!({"ingest_paused": true}));
    assert_eq!(replaced["monthly_spans"], Value::Null);
    assert_eq!(replaced["ingest_paused"], json!(true));
    // The default tenant is spelled `_default` in a path, "" in a body.
    let default = set_limits(&server, "_default", json!({"monthly_spans": 7}));
    assert_eq!(default["tenant"], json!(""));

    for (path, body, needle) in [
        (
            "/v1/admin/tenants/Not-Valid/limits",
            json!({}),
            "tenant must be",
        ),
        (
            "/v1/admin/tenants/acme/limits",
            json!({"monthly_span": 1}),
            "unknown field",
        ),
        (
            "/v1/admin/tenants/acme/limits",
            json!({"monthly_spans": -1}),
            "invalid value",
        ),
    ] {
        let (status, refusal) = call(&server, "root", "POST", path, Some(&body));
        assert_eq!(status, 400, "{path} {body}: {refusal}");
        assert!(
            refusal["error"]
                .as_str()
                .unwrap_or_default()
                .contains(needle),
            "{refusal}"
        );
    }

    server.restart();
    let (_, after) = call(
        &server,
        "root",
        "GET",
        "/v1/admin/tenants/acme/limits",
        None,
    );
    assert_eq!(
        after["ingest_paused"],
        json!(true),
        "limits.json survived kill -9"
    );
    let (_, after) = call(
        &server,
        "root",
        "GET",
        "/v1/admin/tenants/_default/limits",
        None,
    );
    assert_eq!(after["monthly_spans"], json!(7));
}

#[test]
fn a_monthly_span_quota_refuses_whole_batches_with_429_and_retry_after() {
    let server = Server::spawn_with_env(&[], &[("TRAZA_TOKENS", TOKENS)]);
    set_limits(&server, "acme", json!({"monthly_spans": 3}));

    let (status, body) = call(
        &server,
        "acme-rw",
        "POST",
        "/v1/spans",
        Some(&spans("t1", 2)),
    );
    assert_eq!(status, 200, "{body}");

    let authorization = "Bearer acme-rw";
    let encoded = serde_json::to_vec(&spans("t2", 2)).expect("encodes");
    let (status, headers, text) = server.request(
        "POST",
        "/v1/spans",
        Some(&encoded),
        &[("Authorization", authorization)],
    );
    assert_eq!(status, 429, "{text}");
    let refusal: Value = serde_json::from_str(&text).expect("json");
    let window = refusal["quota"]["window"]
        .as_str()
        .expect("window")
        .to_owned();
    assert_eq!(window.len(), 7, "YYYY-MM: {window}");
    let resets_at = refusal["quota"]["resets_at"].as_str().expect("resets_at");
    assert!(
        resets_at.ends_with("-01T00:00:00Z"),
        "resets at the first instant of a month: {resets_at}"
    );
    assert_eq!(
        refusal,
        json!({"error": "quota exceeded: monthly_spans for tenant acme",
               "quota": {"tenant": "acme", "dimension": "monthly_spans", "limit": 3,
                         "used": 2, "window": window, "resets_at": resets_at}})
    );
    let retry_after: u64 = header(&headers, "retry-after")
        .expect("Retry-After")
        .parse()
        .expect("seconds");
    assert!(
        (1..=31 * 86_400).contains(&retry_after),
        "seconds until next month: {retry_after}"
    );
    // Whole batch refused: neither span of t2 exists.
    let (status, _) = call(&server, "root", "GET", "/v1/traces/t2", None);
    assert_eq!(status, 404);

    // Exactly at the limit is allowed; past it is not.
    let (status, _) = call(
        &server,
        "acme-rw",
        "POST",
        "/v1/spans",
        Some(&spans("t3", 1)),
    );
    assert_eq!(status, 200);
    let (status, _) = call(
        &server,
        "acme-rw",
        "POST",
        "/v1/spans",
        Some(&spans("t4", 1)),
    );
    assert_eq!(status, 429);

    // Another tenant is untouched by acme's quota.
    let (status, _) = call(
        &server,
        "bigco-rw",
        "POST",
        "/v1/spans",
        Some(&spans("t5", 5)),
    );
    assert_eq!(status, 200);

    // OTLP answers the same 429 JSON, for JSON and protobuf alike.
    let otlp = json!({"resourceSpans": [{
        "resource": {"attributes": [{"key": "service.name", "value": {"stringValue": "svc"}}]},
        "scopeSpans": [{"spans": [{
            "traceId": "0102030405060708090a0b0c0d0e0f10", "spanId": "0102030405060708",
            "name": "op", "startTimeUnixNano": "1000", "endTimeUnixNano": "2000"}]}]}]});
    let (status, body) = call(&server, "acme-rw", "POST", "/v1/traces", Some(&otlp));
    assert_eq!(status, 429, "{body}");
    assert_eq!(body["quota"]["dimension"], json!("monthly_spans"));
    let (status, headers, text) = server.request(
        "POST",
        "/v1/traces",
        Some(&protobuf_export()),
        &[
            ("Authorization", authorization),
            ("Content-Type", "application/x-protobuf"),
        ],
    );
    assert_eq!(status, 429, "{text}");
    assert!(header(&headers, "retry-after").is_some());
    let refusal: Value = serde_json::from_str(&text).expect("a protobuf client gets JSON too");
    assert_eq!(refusal["quota"]["tenant"], json!("acme"));

    let row = usage_row(&server, "acme");
    assert_eq!(row["month"]["spans"], json!(3));
    assert_eq!(row["lifetime"]["spans"], json!(3));
    assert_eq!(row["lifetime"]["batches"], json!(2));
    assert_eq!(
        row["lifetime"]["rejected"],
        json!(5),
        "2 + 1 native spans, then 1 OTLP/JSON and 1 OTLP/protobuf span, were refused"
    );
    assert_eq!(row["state"], json!("capped"));
    assert_eq!(row["limits"]["monthly_spans"], json!(3));
}

#[test]
fn a_paused_tenant_is_refused_ingest_but_never_its_reads_exports_or_erasure() {
    let server = Server::spawn_with_env(&[], &[("TRAZA_TOKENS", TOKENS)]);
    let (status, _) = call(
        &server,
        "acme-rw",
        "POST",
        "/v1/spans",
        Some(&spans("kept", 2)),
    );
    assert_eq!(status, 200);
    set_limits(&server, "acme", json!({"ingest_paused": true}));

    let encoded = serde_json::to_vec(&spans("late", 1)).expect("encodes");
    let (status, headers, text) = server.request(
        "POST",
        "/v1/spans",
        Some(&encoded),
        &[("Authorization", "Bearer acme-rw")],
    );
    assert_eq!(status, 429, "{text}");
    assert_eq!(header(&headers, "retry-after"), Some("60"));
    let refusal: Value = serde_json::from_str(&text).expect("json");
    assert_eq!(
        refusal["error"],
        json!("quota exceeded: paused for tenant acme")
    );
    assert_eq!(refusal["quota"]["dimension"], json!("paused"));
    assert_eq!(refusal["quota"]["limit"], Value::Null);
    assert_eq!(refusal["quota"]["resets_at"], Value::Null);

    // Reads, export and erasure all still work for the paused tenant.
    let (status, body) = call(&server, "acme-ro", "GET", "/v1/spans", None);
    assert_eq!(status, 200);
    assert_eq!(body["spans"].as_array().map(Vec::len), Some(2));
    let (status, _, export) = server.request(
        "GET",
        "/v1/export",
        None,
        &[("Authorization", "Bearer acme-ro")],
    );
    assert_eq!(status, 200);
    assert_eq!(export.lines().filter(|line| !line.is_empty()).count(), 2);
    let (status, body) = call(
        &server,
        "root",
        "POST",
        "/v1/erasures",
        Some(&json!({"subject": {"kind": "trace", "trace_id": "kept", "tenant": "acme"}})),
    );
    assert_eq!(status, 200, "{body}");

    let row = usage_row(&server, "acme");
    assert_eq!(row["state"], json!("paused"));
    assert_eq!(row["lifetime"]["rejected"], json!(1));

    // Lifting the pause is just another replace.
    set_limits(&server, "acme", json!({}));
    let (status, _) = call(
        &server,
        "acme-rw",
        "POST",
        "/v1/spans",
        Some(&spans("late", 1)),
    );
    assert_eq!(status, 200);
}

#[test]
fn bytes_are_charged_as_sent_before_offload_and_enforced_monthly() {
    let server = Server::spawn_with_env(
        &["--payload-threshold-bytes", "1024"],
        &[("TRAZA_TOKENS", TOKENS)],
    );
    let big = json!([{"trace_id": "big", "span_id": "s1", "name": "llm", "service": "svc",
                      "start_time_ns": 1_000u64, "end_time_ns": 2_000u64,
                      "attributes": {"llm.prompt": "p".repeat(20_000)}}]);
    // What is charged is the span as Traza serializes it after the bound
    // credential stamps its tenant — computed here the same way.
    let mut expected: Vec<Span> = serde_json::from_value(big.clone()).expect("spans");
    expected[0].tenant = "acme".into();
    let expected_bytes = serde_json::to_vec(&expected[0]).expect("encodes").len() as u64;

    let (status, body) = call(&server, "acme-rw", "POST", "/v1/spans", Some(&big));
    assert_eq!(status, 200, "{body}");
    let (_, stored) = call(&server, "root", "GET", "/v1/traces/big", None);
    assert!(
        stored["spans"][0]["attributes"]["llm.prompt"]["$payload"].is_string(),
        "the prompt was offloaded: {stored}"
    );
    let row = usage_row(&server, "acme");
    assert_eq!(row["lifetime"]["bytes"], json!(expected_bytes));
    assert_eq!(row["month"]["bytes"], json!(expected_bytes));
    assert!(
        expected_bytes > 20_000,
        "pre-offload size, not the reference's"
    );

    // Room for 110% of one span: the month is past 80% (a warning), and a
    // second copy does not fit.
    set_limits(
        &server,
        "acme",
        json!({"monthly_bytes": expected_bytes + expected_bytes / 10}),
    );
    let (status, body) = call(&server, "acme-rw", "POST", "/v1/spans", Some(&big));
    assert_eq!(status, 429, "{body}");
    assert_eq!(body["quota"]["dimension"], json!("monthly_bytes"));
    assert_eq!(body["quota"]["used"], json!(expected_bytes));
    assert_eq!(usage_row(&server, "acme")["state"], json!("warning"));
}

#[test]
fn usage_is_scoped_by_binding_and_survives_a_crash_after_a_flush() {
    let mut server = Server::spawn_with_env(&[], &[("TRAZA_TOKENS", TOKENS)]);
    let (status, _) = call(
        &server,
        "acme-rw",
        "POST",
        "/v1/spans",
        Some(&spans("a", 3)),
    );
    assert_eq!(status, 200);
    let (status, _) = call(
        &server,
        "bigco-rw",
        "POST",
        "/v1/spans",
        Some(&spans("b", 1)),
    );
    assert_eq!(status, 200);
    set_limits(&server, "quiet", json!({"monthly_spans": 10}));

    // Unbound: every tenant with counters or limits, in tenant order.
    let (status, all) = call(&server, "root", "GET", "/v1/usage", None);
    assert_eq!(status, 200);
    assert!(all["as_of_ns"].as_u64().is_some_and(|ns| ns > 0));
    let names: Vec<&str> = all["tenants"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| row["tenant"].as_str().expect("name"))
        .collect();
    assert_eq!(names, vec!["acme", "bigco", "quiet"]);
    let quiet = &all["tenants"][2];
    assert_eq!(quiet["lifetime"]["spans"], json!(0));
    assert_eq!(quiet["state"], json!("ok"));

    // Bound: its own row, read with a ro token — usage is not an operator
    // surface — and naming anyone else is refused.
    let (status, own) = call(&server, "acme-ro", "GET", "/v1/usage", None);
    assert_eq!(status, 200);
    let rows = own["tenants"].as_array().expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["tenant"], json!("acme"));
    assert_eq!(rows[0]["lifetime"]["spans"], json!(3));
    let (status, refusal) = call(&server, "acme-ro", "GET", "/v1/usage?tenant=bigco", None);
    assert_eq!((status, &refusal["required"]), (403, &json!("unbound")));
    let (status, _) = call(&server, "acme-ro", "GET", "/v1/usage?tenant=acme", None);
    assert_eq!(status, 200);
    let (status, _) = call(&server, "root", "GET", "/v1/usage?bogus=1", None);
    assert_eq!(status, 400);

    // The Prometheus series carry the tenant label.
    let (status, _, metrics) = server.request(
        "GET",
        "/v1/metrics",
        None,
        &[("Authorization", "Bearer root")],
    );
    assert_eq!(status, 200);
    assert!(
        metrics.contains("traza_tenant_spans_ingested_total{tenant=\"acme\"} 3"),
        "{metrics}"
    );
    assert!(
        metrics.contains("traza_quota_rejected_total{tenant=\"bigco\"} 0"),
        "{metrics}"
    );

    // A flush writes the counters; kill -9 then keeps them.
    let (status, _) = call(&server, "root", "POST", "/v1/flush", None);
    assert_eq!(status, 200);
    assert!(server.data_dir.join("usage.json").exists());
    server.restart();
    let row = usage_row(&server, "acme");
    assert_eq!(row["lifetime"]["spans"], json!(3));
    assert_eq!(row["month"]["spans"], json!(3));
}

#[test]
fn the_maintenance_tick_writes_the_counters_without_a_flush() {
    let mut server = Server::spawn(&[]);
    let (status, _) = server.json("POST", "/v1/spans", Some(&spans("t", 2)), &[]);
    assert_eq!(status, 200);
    // The tick is every 5 s; wait for its write rather than for a duration.
    let path = server.data_dir.join("usage.json");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "no tick wrote usage.json"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    server.restart();
    let (_, body) = server.json("GET", "/v1/usage?tenant=", None, &[]);
    assert_eq!(body["tenants"][0]["tenant"], json!(""));
    assert_eq!(body["tenants"][0]["lifetime"]["spans"], json!(2));
}

/// A one-span OTLP `ExportTraceServiceRequest`, hand-encoded: the fields
/// the decoder needs (trace id 1, span id 2, name 5, start 7, end 8), inside
/// scope spans (2), inside resource spans (2), inside the request (1).
fn protobuf_export() -> Vec<u8> {
    fn varint(mut value: u64, out: &mut Vec<u8>) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }
    fn bytes_field(field: u64, bytes: &[u8], out: &mut Vec<u8>) {
        varint(field << 3 | 2, out);
        varint(bytes.len() as u64, out);
        out.extend_from_slice(bytes);
    }
    fn fixed64_field(field: u64, value: u64, out: &mut Vec<u8>) {
        varint(field << 3 | 1, out);
        out.extend_from_slice(&value.to_le_bytes());
    }
    let mut span = Vec::new();
    bytes_field(1, &[0xAB; 16], &mut span);
    bytes_field(2, &[0x01; 8], &mut span);
    bytes_field(5, b"op", &mut span);
    fixed64_field(7, 1_000, &mut span);
    fixed64_field(8, 2_000, &mut span);
    let mut scope_spans = Vec::new();
    bytes_field(2, &span, &mut scope_spans);
    let mut resource_spans = Vec::new();
    bytes_field(2, &scope_spans, &mut resource_spans);
    let mut request = Vec::new();
    bytes_field(1, &resource_spans, &mut request);
    request
}

static NEXT: AtomicU64 = AtomicU64::new(0);

fn scratch_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "traza-quotas-it-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

fn old_span(tenant: &str, id: &str) -> Span {
    Span {
        trace_id: format!("t-{tenant}"),
        span_id: id.into(),
        tenant: tenant.into(),
        parent_span_id: None,
        name: "op".into(),
        // At the epoch: past any window a test sets.
        start_time_ns: 1,
        end_time_ns: 2,
        status: String::new(),
        service: "svc".into(),
        attributes: Default::default(),
        events: Vec::new(),
        links: Vec::new(),
        extra: Default::default(),
    }
}

#[test]
fn a_runtime_ttl_overrides_the_flags_for_its_tenant_only() {
    let dir = scratch_dir();
    let config = Config {
        // The flags: everyone keeps a day, `exempt` keeps forever.
        ttl_seconds: Some(86_400),
        tenant_ttl_seconds: [("exempt".to_owned(), 0)].into_iter().collect(),
        ..Config::default()
    };
    let store = Store::open(&dir, config).expect("opens");
    for tenant in ["exempt", "keeper", "plain"] {
        store.ingest(old_span(tenant, "s1")).expect("ingests");
    }
    store.flush().expect("flushes");

    // Runtime: `exempt` gets a window after all; `keeper` is exempted.
    store
        .set_tenant_limits(
            "exempt",
            TenantLimits {
                ttl_seconds: Some(3_600),
                ..TenantLimits::default()
            },
        )
        .expect("sets");
    store
        .set_tenant_limits(
            "keeper",
            TenantLimits {
                ttl_seconds: Some(0),
                ..TenantLimits::default()
            },
        )
        .expect("sets");
    let effective = |tenant: &str| store.effective_ttl(tenant).expect("resolves");
    assert_eq!(effective("exempt").ttl_seconds, Some(3_600));
    assert_eq!(effective("exempt").source, RetentionSource::Limits);
    assert_eq!(effective("keeper").ttl_seconds, None);
    assert_eq!(effective("keeper").source, RetentionSource::Limits);
    assert_eq!(effective("plain").ttl_seconds, Some(86_400));
    assert_eq!(effective("plain").source, RetentionSource::Global);

    store.compact_expired().expect("expires");
    let left = |tenant: &str| {
        store
            .get_trace_in(Some(tenant), &format!("t-{tenant}"))
            .expect("reads")
            .len()
    };
    assert_eq!(
        left("exempt"),
        0,
        "the runtime window beat the flag's exemption"
    );
    assert_eq!(
        left("keeper"),
        1,
        "the runtime exemption beat the global window"
    );
    assert_eq!(left("plain"), 0);

    // A null TTL falls back to the flags again.
    store
        .set_tenant_limits("keeper", TenantLimits::default())
        .expect("sets");
    assert_eq!(effective("keeper").source, RetentionSource::Global);
    store.compact_expired().expect("expires");
    assert_eq!(left("keeper"), 0);
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_runtime_ttl_is_honoured_on_a_store_with_no_flags_at_all() {
    let dir = scratch_dir();
    let store = Store::open(&dir, Config::default()).expect("opens");
    store.ingest(old_span("acme", "s1")).expect("ingests");
    store.ingest(old_span("other", "s1")).expect("ingests");
    assert_eq!(store.compact_expired().expect("no policy"), 0);
    assert_eq!(
        store.effective_ttl("acme").expect("resolves").source,
        RetentionSource::None
    );
    store
        .set_tenant_limits(
            "acme",
            TenantLimits {
                ttl_seconds: Some(60),
                ..TenantLimits::default()
            },
        )
        .expect("sets");
    store.compact_expired().expect("expires");
    assert!(store
        .get_trace_in(Some("acme"), "t-acme")
        .expect("reads")
        .is_empty());
    assert_eq!(
        store
            .get_trace_in(Some("other"), "t-other")
            .expect("reads")
            .len(),
        1,
        "an unlisted tenant with no global window keeps everything"
    );
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}
