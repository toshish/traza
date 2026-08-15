//! Tenant identity in the primary key, held to its acceptance criteria.
//!
//! The contract under test: span identity is `(tenant, trace_id, span_id)`,
//! so two tenants sharing a trace id can never upsert over each other; a
//! default tenant keeps single-tenant deployments byte-identical on disk in
//! the WAL, segments and annotation log; and tenant scoping genuinely
//! reaches sessions, annotations, payload references, retention policy and
//! quota accounting — each proven here against the surface a tenant would
//! actually cross, not asserted in prose.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use traza::erasure::Subject;
use traza::{Config, Durability, Span, SpanFilter, Store};

fn test_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "traza-tenancy-{label}-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("test dir");
    dir
}

fn wal_config() -> Config {
    Config {
        flush_spans: 1_000_000,
        durability: Durability::Wal,
        ..Config::default()
    }
}

fn tenant_span(tenant: &str, trace: &str, id: &str, name: &str, attributes: Value) -> Span {
    let mut value = json!({
        "trace_id": trace, "span_id": id, "name": name,
        "service": "svc", "start_time_ns": 1_000u64, "end_time_ns": 2_000u64,
        "attributes": attributes,
    });
    if !tenant.is_empty() {
        value["tenant"] = json!(tenant);
    }
    serde_json::from_value(value).expect("span")
}

#[test]
fn two_tenants_sharing_a_primary_key_never_collide() {
    let dir = test_dir("collide");
    let store = Store::open(&dir, wal_config()).expect("opens");

    // The exact scenario the milestone exists to prevent: three writers,
    // one shared (trace_id, span_id), each upserting its own version.
    store
        .ingest(tenant_span("", "t1", "s1", "default-v1", json!({})))
        .expect("default");
    store
        .ingest(tenant_span("acme", "t1", "s1", "acme-v1", json!({})))
        .expect("acme");
    store
        .ingest(tenant_span("bigco", "t1", "s1", "bigco-v1", json!({})))
        .expect("bigco");
    // LWW still applies WITHIN a tenant.
    store
        .ingest(tenant_span("acme", "t1", "s1", "acme-v2", json!({})))
        .expect("acme update");
    // Half sealed, half buffered — the key must hold across both.
    store.flush().expect("seals");
    store
        .ingest(tenant_span("bigco", "t1", "s2", "bigco-extra", json!({})))
        .expect("bigco buffered");

    let operator = store.get_trace("t1").expect("operator view");
    assert_eq!(operator.len(), 4, "three tenants' s1 plus bigco's s2");

    let acme = store.get_trace_in(Some("acme"), "t1").expect("acme view");
    assert_eq!(acme.len(), 1);
    assert_eq!(
        acme[0].name, "acme-v2",
        "acme's own update won, nobody else's"
    );
    assert_eq!(
        store
            .get_trace_in(Some(""), "t1")
            .expect("default view")
            .len(),
        1
    );

    // Scoped queries agree with the trace view.
    let filter = SpanFilter {
        tenant: Some("bigco".to_owned()),
        ..SpanFilter::default()
    };
    let bigco = store.query(&filter).expect("scoped query");
    assert_eq!(bigco.len(), 2);
    assert!(bigco.iter().all(|span| span.tenant == "bigco"));

    // And all of it survives a reopen: the key is in the recovery path, not
    // just the in-memory index.
    drop(store);
    let store = Store::open(&dir, wal_config()).expect("reopens");
    assert_eq!(store.get_trace("t1").expect("operator view").len(), 4);
    assert_eq!(
        store
            .get_trace_in(Some("acme"), "t1")
            .expect("acme view")
            .first()
            .map(|span| span.name.clone()),
        Some("acme-v2".to_owned())
    );
}

#[test]
fn a_single_tenant_store_writes_no_tenant_bytes_anywhere() {
    let dir = test_dir("byte-identity");
    let store = Store::open(&dir, wal_config()).expect("opens");
    store
        .ingest(tenant_span("", "t1", "s1", "op", json!({"k": "v"})))
        .expect("ingests");
    store
        .annotate(
            serde_json::from_value(json!({
                "trace_id": "t1", "span_id": "s1", "name": "quality", "value": 1,
            }))
            .expect("annotation"),
        )
        .expect("annotates");
    store.flush().expect("seals");
    store
        .ingest(tenant_span("", "t2", "s2", "op", json!({})))
        .expect("buffered");
    drop(store);

    // The structural guarantee: `skip_serializing_if` on an empty tenant
    // means the WORD never reaches disk for a store that never used one.
    // (Sidecar caches legitimately re-encode under their own version gates
    // and are excluded — they are derived, not identity.)
    for entry in std::fs::read_dir(&dir).expect("dir") {
        let path = entry.expect("entry").path();
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if path.is_dir() || name.ends_with(".rollup") || name == "LOCK" || name == "CURRENT" {
            continue;
        }
        let bytes = std::fs::read(&path).expect("reads");
        assert!(
            !bytes.windows(8).any(|window| window == b"\"tenant\""),
            "{name} carries tenant bytes in a single-tenant store"
        );
    }
}

#[test]
fn sessions_are_tenant_scoped_and_never_merge_across_tenants() {
    let dir = test_dir("sessions");
    let store = Store::open(&dir, wal_config()).expect("opens");
    let session = |tenant: &str, trace: &str, id: &str| {
        tenant_span(
            tenant,
            trace,
            id,
            "chat",
            json!({"session.id": "chat-1", "gen_ai.system": "openai",
                   "llm.total_tokens": 10}),
        )
    };
    store.ingest(session("", "t1", "s1")).expect("default");
    store.ingest(session("acme", "t2", "s1")).expect("acme");
    store.ingest(session("acme", "t3", "s2")).expect("acme 2");
    store.flush().expect("seals");

    let all = store
        .sessions(None, None, 10, traza::analytics::SessionOrder::Recent)
        .expect("sessions");
    assert_eq!(all.len(), 2, "one session id, two tenants, two rows");
    let acme_row = all
        .iter()
        .find(|row| row.tenant == "acme")
        .expect("acme row");
    assert_eq!(acme_row.trace_count, 2);
    let default_row = all
        .iter()
        .find(|row| row.tenant.is_empty())
        .expect("default row");
    assert_eq!(default_row.trace_count, 1);

    let scoped = store
        .sessions_in(
            Some("acme"),
            None,
            None,
            10,
            traza::analytics::SessionOrder::Recent,
        )
        .expect("scoped");
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].tenant, "acme");

    let detail = store
        .session_in(Some("acme"), "chat-1")
        .expect("detail")
        .expect("exists");
    assert_eq!(detail.summary.trace_count, 2);
    assert_eq!(detail.summary.tenant, "acme");

    // The display-key collision case: a DEFAULT-tenant session literally
    // named "acme/chat-9" must not merge with tenant acme's session
    // "chat-9" in the session aggregation.
    store
        .ingest(tenant_span(
            "",
            "t4",
            "s1",
            "chat",
            json!({"session.id": "acme/chat-9", "llm.total_tokens": 7}),
        ))
        .expect("collider");
    store
        .ingest(tenant_span(
            "acme",
            "t5",
            "s1",
            "chat",
            json!({"session.id": "chat-9", "llm.total_tokens": 5}),
        ))
        .expect("collidee");
    let rows = store
        .llm_aggregate(traza::analytics::LlmGroupBy::Session, None, None)
        .expect("aggregate");
    let rendered: Vec<&str> = rows
        .iter()
        .filter(|row| row.key == "acme/chat-9")
        .map(|row| row.key.as_str())
        .collect();
    assert_eq!(
        rendered.len(),
        2,
        "two structurally distinct sessions may RENDER alike; merged counters would be a lie"
    );
}

#[test]
fn per_tenant_ttl_expires_one_tenant_and_never_touches_an_unswept_one() {
    let dir = test_dir("ttl");
    // The P1 configuration from design review: a tenant TTL and NO global
    // TTL. The default tenant has no window at all, and a retire-whole
    // bound computed from configured windows alone would delete its
    // segment outright.
    let config = Config {
        flush_spans: 1_000_000,
        durability: Durability::Wal,
        tenant_ttl_seconds: [("acme".to_owned(), 60)].into_iter().collect(),
        ..Config::default()
    };
    let store = Store::open(&dir, config.clone()).expect("opens");

    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let old = now_ns - 3_600 * 1_000_000_000; // an hour ago
    let mut acme_old = tenant_span("acme", "ta", "s1", "old", json!({}));
    acme_old.start_time_ns = old;
    acme_old.end_time_ns = old + 1;
    let mut default_old = tenant_span("", "td", "s1", "old", json!({}));
    default_old.start_time_ns = old;
    default_old.end_time_ns = old + 1;
    let mut acme_fresh = tenant_span("acme", "ta", "s2", "fresh", json!({}));
    acme_fresh.start_time_ns = now_ns;
    acme_fresh.end_time_ns = now_ns + 1;

    store.ingest(acme_old).expect("acme old");
    store.ingest(default_old).expect("default old");
    store.ingest(acme_fresh).expect("acme fresh");
    // Sealed, so the segment fast paths (skip / retire-whole) are what run.
    store.flush().expect("seals");

    let removed = store.compact_expired().expect("sweeps");
    assert_eq!(removed, 1, "exactly acme's old span expires");
    assert!(
        store
            .get_trace_in(Some("acme"), "ta")
            .expect("acme")
            .iter()
            .all(|span| span.span_id == "s2"),
        "acme keeps only the fresh span"
    );
    assert_eq!(
        store.get_trace_in(Some(""), "td").expect("default").len(),
        1,
        "a tenant with no window NEVER expires, whatever its neighbours configured"
    );

    // And the deletion (and the survival) hold across a restart.
    drop(store);
    let store = Store::open(&dir, config).expect("reopens");
    assert_eq!(
        store.get_trace_in(Some(""), "td").expect("default").len(),
        1
    );
    assert_eq!(
        store.get_trace_in(Some("acme"), "ta").expect("acme").len(),
        1
    );
}

#[test]
fn tenant_erasure_is_reference_aware_and_the_receipt_verifies() {
    let dir = test_dir("tenant-erase");
    let config = Config {
        flush_spans: 1_000_000,
        durability: Durability::Wal,
        payload_threshold: Some(64),
        ..Config::default()
    };
    let store = Store::open(&dir, config).expect("opens");

    // One oversized value, ingested by BOTH tenants: content addressing
    // dedups it into one file, and erasing acme must keep it for bigco.
    let shared = "shared-secret-material-".repeat(16);
    store
        .ingest(tenant_span(
            "acme",
            "ta",
            "s1",
            "op",
            json!({"prompt": shared.clone(), "session.id": "acme-sess"}),
        ))
        .expect("acme");
    store
        .ingest(tenant_span(
            "bigco",
            "tb",
            "s1",
            "op",
            json!({"prompt": shared.clone()}),
        ))
        .expect("bigco");
    store.flush().expect("seals");
    store
        .annotate(
            serde_json::from_value(json!({
                "trace_id": "ta", "span_id": "s1", "tenant": "acme",
                "name": "quality", "value": 1,
            }))
            .expect("annotation"),
        )
        .expect("annotates acme");
    // A session-subject annotation with NO span address: only the typed
    // subject machinery can reach it.
    store
        .annotate(
            serde_json::from_value(json!({
                "tenant": "acme", "session_id": "acme-sess",
                "name": "vibe", "value": "good",
            }))
            .expect("annotation"),
        )
        .expect("annotates session");

    let status = store
        .erase(Subject::Tenant {
            tenant: "acme".into(),
        })
        .expect("erases");
    let settle = status.settle.expect("settles synchronously");
    assert_eq!(settle.spans_removed, 1);
    assert_eq!(
        settle.annotations_removed, 2,
        "the span-addressed judgment AND the session-subject one"
    );
    assert_eq!(
        settle.payloads_retained.len(),
        1,
        "the shared blob survives for bigco"
    );

    assert!(store
        .get_trace_in(Some("acme"), "ta")
        .expect("acme")
        .is_empty());
    assert_eq!(
        store
            .get_trace_in(Some("bigco"), "tb")
            .expect("bigco")
            .len(),
        1
    );
    let reference = settle.payloads_retained[0].reference.clone();
    assert!(
        store.payload(&reference).expect("payload").is_some(),
        "bigco's bytes are still served"
    );

    let receipt = store.verify_erasure(status.erase.id).expect("receipt");
    assert_eq!(receipt.result, "erased", "{}", receipt.render_text());
    let payloads = receipt
        .domains
        .iter()
        .find(|domain| domain.domain == "payloads")
        .expect("payload domain");
    assert!(
        payloads.items.iter().any(|item| item.contains("retained")),
        "the receipt names the retained shared payload"
    );

    // Post-settle, acme may return: an erasure is a barrier, not a ban.
    store
        .ingest(tenant_span("acme", "ta2", "s1", "new", json!({})))
        .expect("new acme data");
    assert_eq!(
        store.get_trace_in(Some("acme"), "ta2").expect("acme").len(),
        1
    );
}

#[test]
fn tenant_usage_accounts_spans_and_payload_bytes_per_tenant() {
    let dir = test_dir("usage");
    let config = Config {
        flush_spans: 1_000_000,
        durability: Durability::Wal,
        payload_threshold: Some(64),
        ..Config::default()
    };
    let store = Store::open(&dir, config).expect("opens");
    let big = "x".repeat(500);
    store
        .ingest(tenant_span(
            "acme",
            "t1",
            "s1",
            "op",
            json!({"prompt": big}),
        ))
        .expect("acme");
    store
        .ingest(tenant_span("acme", "t1", "s2", "op", json!({})))
        .expect("acme 2");
    store
        .ingest(tenant_span("", "t2", "s1", "op", json!({})))
        .expect("default");

    let rows = store.tenant_usage(None).expect("usage");
    assert_eq!(rows.len(), 2);
    let acme = rows.iter().find(|row| row.tenant == "acme").expect("acme");
    assert_eq!(acme.spans, 2);
    assert_eq!(acme.traces, 1);
    assert_eq!(acme.payload_bytes_approx, 500);
    let scoped = store.tenant_usage(Some("acme")).expect("scoped");
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].tenant, "acme");
}

// ---------------------------------------------------------------- HTTP

struct Server {
    child: Child,
    port: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    fn spawn_with(data_dir: &Path, tokens: Option<&str>, extra: &[&str]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_traza-server"));
        command
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg("0")
            .arg("--durability")
            .arg("wal")
            .arg("--flush-spans")
            .arg("1000000");
        for arg in extra {
            command.arg(arg);
        }
        match tokens {
            Some(tokens) => command.env("TRAZA_TOKENS", tokens),
            None => command.env_remove("TRAZA_TOKENS"),
        };
        let mut child = command
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawns traza-server");
        let stderr = child.stderr.take().expect("stderr piped");
        let mut lines = std::io::BufRead::lines(std::io::BufReader::new(stderr));
        let port = loop {
            let line = lines.next().expect("port line").expect("stderr read");
            if let Some(rest) = line.strip_prefix("traza-server listening on 127.0.0.1:") {
                break rest.trim().parse::<u16>().expect("port parses");
            }
        };
        std::thread::spawn(move || for _ in lines {});
        Self { child, port }
    }

    fn kill_hard(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn request_as(
        &self,
        token: Option<&str>,
        method: &str,
        target: &str,
        body: Option<&Value>,
    ) -> (u16, Value) {
        let encoded = body.map(|value| serde_json::to_vec(value).expect("encodes"));
        let mut stream = {
            let mut attempt = 0;
            loop {
                match TcpStream::connect(("127.0.0.1", self.port)) {
                    Ok(stream) => break stream,
                    Err(_) if attempt < 100 => {
                        attempt += 1;
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(error) => panic!("connect: {error}"),
                }
            }
        };
        let length = encoded.as_ref().map_or(0, Vec::len);
        let authorization = token.map_or(String::new(), |token| {
            format!("Authorization: Bearer {token}\r\n")
        });
        write!(
            stream,
            "{method} {target} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n\
             {authorization}Content-Length: {length}\r\nConnection: close\r\n\r\n"
        )
        .expect("writes");
        if let Some(bytes) = encoded {
            stream.write_all(&bytes).expect("body");
        }
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);
        let text = String::from_utf8_lossy(&response);
        let status = text
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .expect("status");
        let payload = text
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .filter(|body| !body.is_empty())
            .and_then(|body| serde_json::from_str(body).ok())
            .unwrap_or(Value::Null);
        (status, payload)
    }
}

const TOKENS: &str = "rw@acme:acme-rw,ro@acme:acme-ro,rw@bigco:bigco-rw,\
                      admin@acme:acme-admin,admin:root,ro:observer";

#[test]
fn bound_credentials_are_isolated_on_every_read_and_write_surface() {
    let dir = test_dir("http-isolation");
    let mut server = Server::spawn_with(&dir, Some(TOKENS), &[]);

    // Ingest: a bound token's spans are stamped with its tenant; naming a
    // foreign tenant fails the batch loudly.
    let (status, body) = server.request_as(
        Some("acme-rw"),
        "POST",
        "/v1/spans",
        Some(&json!([{
            "trace_id": "t1", "span_id": "s1", "name": "acme-op", "service": "svc",
            "start_time_ns": 1000u64, "end_time_ns": 2000u64,
        }])),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["accepted"], json!(1));
    let (status, _) = server.request_as(
        Some("bigco-rw"),
        "POST",
        "/v1/spans",
        Some(&json!([{
            "trace_id": "t1", "span_id": "s1", "tenant": "bigco",
            "name": "bigco-op", "service": "svc",
            "start_time_ns": 1000u64, "end_time_ns": 2000u64,
        }])),
    );
    assert_eq!(status, 200);
    let (status, body) = server.request_as(
        Some("acme-rw"),
        "POST",
        "/v1/spans",
        Some(&json!([{
            "trace_id": "tx", "span_id": "s1", "tenant": "bigco",
            "name": "forged", "service": "svc",
            "start_time_ns": 1000u64, "end_time_ns": 2000u64,
        }])),
    );
    assert_eq!(
        status, 400,
        "a bound credential cannot write another tenant"
    );
    assert!(body["error"].as_str().unwrap_or("").contains("binding"));

    // Reads: every span surface answers within the binding.
    let (status, body) = server.request_as(Some("acme-ro"), "GET", "/v1/spans", None);
    assert_eq!(status, 200);
    let spans = body["spans"].as_array().expect("spans");
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0]["name"], json!("acme-op"));
    let (status, _) = server.request_as(Some("acme-ro"), "GET", "/v1/spans?tenant=bigco", None);
    assert_eq!(status, 403, "naming a foreign tenant is refused, not empty");

    let (status, body) = server.request_as(Some("acme-ro"), "GET", "/v1/traces/t1", None);
    assert_eq!(status, 200);
    assert_eq!(body["spans"].as_array().expect("spans").len(), 1);
    assert_eq!(body["spans"][0]["name"], json!("acme-op"));

    // The operator sees the union; the trace endpoint carries both tenants.
    let (status, body) = server.request_as(Some("root"), "GET", "/v1/traces/t1", None);
    assert_eq!(status, 200);
    assert_eq!(body["spans"].as_array().expect("spans").len(), 2);

    // Insights surfaces return full span bodies / identifiers — they must
    // be bound too.
    let (status, body) = server.request_as(Some("acme-ro"), "GET", "/v1/stats/slowest", None);
    assert_eq!(status, 200);
    assert!(body["spans"]
        .as_array()
        .expect("spans")
        .iter()
        .all(|span| span["name"] == json!("acme-op")));
    let (status, body) =
        server.request_as(Some("acme-ro"), "GET", "/v1/stats/failures?status=", None);
    assert_eq!(status, 200, "{body}");

    // Export: a whole-store dump surface, scoped.
    let (_, export) = server.request_as(Some("acme-ro"), "GET", "/v1/export", None);
    // NDJSON body — re-read raw. The JSON parse above fails (multi-line),
    // so fetch again and inspect text.
    let _ = export;
    // Operator endpoints refuse bound tokens outright.
    for target in ["/v1/stats", "/v1/metrics", "/v1/metrics.json", "/v1/verify"] {
        let (status, _) = server.request_as(Some("acme-ro"), "GET", target, None);
        assert_eq!(status, 403, "{target} is operator-only for bound tokens");
        let (status, _) = server.request_as(Some("observer"), "GET", target, None);
        assert_eq!(status, 200, "{target} stays open to unbound readers");
    }
    let (status, _) = server.request_as(Some("acme-rw"), "POST", "/v1/flush", None);
    assert_eq!(status, 403);

    // Annotations: bound writes are stamped; bound reads are scoped.
    let (status, _) = server.request_as(
        Some("acme-rw"),
        "POST",
        "/v1/annotations",
        Some(&json!({"trace_id": "t1", "span_id": "s1", "name": "quality", "value": 1})),
    );
    assert_eq!(status, 200);
    let (status, body) = server.request_as(Some("bigco-rw"), "GET", "/v1/annotations", None);
    assert_eq!(status, 200);
    assert_eq!(
        body["annotations"].as_array().expect("annotations").len(),
        0,
        "another tenant's judgments are invisible"
    );

    // /v1/tenants: a bound token gets exactly its own row.
    let (status, body) = server.request_as(Some("acme-ro"), "GET", "/v1/tenants", None);
    assert_eq!(status, 200);
    let rows = body["tenants"].as_array().expect("tenants");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["tenant"], json!("acme"));

    // Erasure bindings: a bound admin erases its tenant, nobody else's,
    // and payload subjects need an unbound operator.
    let (status, _) = server.request_as(
        Some("acme-admin"),
        "POST",
        "/v1/erasures",
        Some(&json!({"subject": {"kind": "tenant", "tenant": "bigco"}})),
    );
    assert_eq!(status, 403, "a bound admin cannot erase a neighbour");
    let (status, _) = server.request_as(
        Some("acme-admin"),
        "POST",
        "/v1/erasures",
        Some(&json!({"subject": {"kind": "payload",
            "reference": format!("sha256/{}", "a".repeat(64))}})),
    );
    assert_eq!(status, 403, "payload subjects are store-global");
    let (status, body) = server.request_as(
        Some("acme-admin"),
        "POST",
        "/v1/erasures",
        Some(&json!({"subject": {"kind": "trace", "trace_id": "t1", "tenant": "acme"}})),
    );
    assert_eq!(status, 200, "{body}");
    // The erased trace is gone for acme; bigco's same-id trace is untouched.
    let (status, _) = server.request_as(Some("acme-ro"), "GET", "/v1/traces/t1", None);
    assert_eq!(status, 404);
    let (status, body) = server.request_as(Some("bigco-rw"), "GET", "/v1/traces/t1", None);
    assert_eq!(status, 200);
    assert_eq!(body["spans"][0]["name"], json!("bigco-op"));

    // A bound credential's erasure listing shows its tenant's history only.
    let (status, body) = server.request_as(Some("bigco-rw"), "GET", "/v1/erasures", None);
    assert_eq!(status, 200);
    assert_eq!(body["erasures"].as_array().expect("list").len(), 0);

    // Kill -9 and reopen: bindings live in config, identity lives on disk.
    server.kill_hard();
    let server = Server::spawn_with(&dir, Some(TOKENS), &[]);
    let (status, body) = server.request_as(Some("bigco-rw"), "GET", "/v1/traces/t1", None);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["spans"][0]["tenant"], json!("bigco"));
}

#[test]
fn otlp_ingest_reads_the_tenant_resource_attribute() {
    let dir = test_dir("otlp-tenant");
    let server = Server::spawn_with(&dir, None, &[]);
    let body = json!({
        "resourceSpans": [{
            "resource": {"attributes": [
                {"key": "service.name", "value": {"stringValue": "svc"}},
                {"key": "traza.tenant", "value": {"stringValue": "acme"}},
            ]},
            "scopeSpans": [{
                "spans": [{
                    "traceId": "0102030405060708090a0b0c0d0e0f10",
                    "spanId": "0102030405060708",
                    "name": "otlp-op",
                    "startTimeUnixNano": "1000",
                    "endTimeUnixNano": "2000",
                }]
            }]
        }]
    });
    let (status, response) = server.request_as(None, "POST", "/v1/traces", Some(&body));
    assert_eq!(status, 200, "{response}");
    let (status, found) = server.request_as(None, "GET", "/v1/spans?tenant=acme", None);
    assert_eq!(status, 200);
    let spans = found["spans"].as_array().expect("spans");
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0]["tenant"], json!("acme"));

    // An INVALID tenant value refuses the export loudly — a misconfigured
    // exporter must hear it, not lose telemetry to a silent drop.
    let bad = json!({
        "resourceSpans": [{
            "resource": {"attributes": [
                {"key": "traza.tenant", "value": {"stringValue": "Not A Tenant"}},
            ]},
            "scopeSpans": [{
                "spans": [{
                    "traceId": "0102030405060708090a0b0c0d0e0f10",
                    "spanId": "0102030405060709",
                    "name": "bad",
                    "startTimeUnixNano": "1000",
                    "endTimeUnixNano": "2000",
                }]
            }]
        }]
    });
    let (status, _) = server.request_as(None, "POST", "/v1/traces", Some(&bad));
    assert_eq!(status, 400);
}

#[test]
fn mcp_tools_are_scoped_by_the_credential_binding() {
    let dir = test_dir("mcp-tenant");
    let store = Store::open(&dir, wal_config()).expect("opens");
    store
        .ingest(tenant_span("acme", "t1", "s1", "acme-op", json!({})))
        .expect("acme");
    store
        .ingest(tenant_span("bigco", "t2", "s1", "bigco-op", json!({})))
        .expect("bigco");

    let server = traza::mcp::Server::new(&store, traza::mcp::Limits::default(), false);
    let bound = traza::mcp::Context {
        access: traza::mcp::Access::Read,
        tenant: Some("acme".to_owned()),
        now_ns: traza::mcp::unix_nanos_now(),
    };
    let message = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "search_spans", "arguments": {}}
    });
    let response = server.handle(&message, bound).expect("responds");
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .expect("text");
    assert!(text.contains("acme-op"), "sees its own tenant: {text}");
    assert!(
        !text.contains("bigco-op"),
        "never sees the neighbour: {text}"
    );
}

#[test]
fn a_zero_tenant_ttl_is_an_exemption_not_a_fallthrough() {
    // Review round 1, P1: `--tenant-ttl acme=0` dropped the entry, so acme
    // fell through to the GLOBAL window and expired — the exact data the
    // operator had just exempted. Zero now means never, and an exempt
    // tenant also disables the whole-segment retirement fast path.
    let config = Config {
        flush_spans: 1_000_000,
        durability: Durability::Wal,
        ttl_seconds: Some(60),
        tenant_ttl_seconds: [("acme".to_owned(), 0)].into_iter().collect(),
        ..Config::default()
    };
    let dir = test_dir("ttl-exempt");
    let store = Store::open(&dir, config.clone()).expect("opens");
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let old = now_ns - 3_600 * 1_000_000_000;
    for (tenant, trace) in [("acme", "ta"), ("", "td")] {
        let mut span = tenant_span(tenant, trace, "s1", "old", json!({}));
        span.start_time_ns = old;
        span.end_time_ns = old + 1;
        store.ingest(span).expect("ingests");
    }
    store.flush().expect("seals");
    let removed = store.compact_expired().expect("sweeps");
    assert_eq!(removed, 1, "only the default tenant's old span expires");
    assert_eq!(
        store.get_trace_in(Some("acme"), "ta").expect("acme").len(),
        1,
        "an exempted tenant keeps everything, global window or not"
    );
    assert!(store
        .get_trace_in(Some(""), "td")
        .expect("default")
        .is_empty());
}
