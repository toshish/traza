//! Deletion with a receipt, held to its own claims.
//!
//! M3's contract has three parts, and each gets its test here rather than an
//! assertion in prose: a published deletion is durable when `CURRENT` moves
//! (proven by reopening, and by killing a real server after its 200);
//! queries never return tombstoned content even before the rewrite runs
//! (proven through a hand-planted pending tombstone); and the erasure is
//! provably absent from every domain afterwards — where "provably" is
//! `verify_erasure`'s receipt, so these tests hold the RECEIPT to its claims
//! too: it must fail on re-delivered data and on a pin that still holds the
//! subject, and pass once they are gone.

use std::collections::HashSet;
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
        "traza-erase-{label}-{}-{nonce}",
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

fn span(trace: &str, id: &str, attributes: Value) -> Span {
    serde_json::from_value(json!({
        "trace_id": trace, "span_id": id, "name": format!("op-{id}"),
        "service": "svc", "start_time_ns": 1_000u64, "end_time_ns": 2_000u64,
        "attributes": attributes,
    }))
    .expect("span")
}

fn keys_of(spans: &[Span]) -> HashSet<(String, String)> {
    spans
        .iter()
        .map(|span| (span.trace_id.clone(), span.span_id.clone()))
        .collect()
}

fn domain<'a>(
    receipt: &'a traza::erasure::Receipt,
    name: &str,
) -> &'a traza::erasure::DomainReport {
    receipt
        .domains
        .iter()
        .find(|domain| domain.domain == name)
        .unwrap_or_else(|| panic!("the receipt names a {name} domain"))
}

#[test]
fn erasing_a_trace_purges_every_domain_and_the_receipt_verifies() {
    let dir = test_dir("trace");
    let store = Store::open(&dir, wal_config()).expect("opens");

    // Two traces: one to erase, one that must be untouched. Half the doomed
    // trace is sealed into a segment, half stays buffered — the purge must
    // reach both.
    store.ingest(span("doomed", "s1", json!({}))).expect("s1");
    store.ingest(span("kept", "k1", json!({}))).expect("k1");
    store.flush().expect("seals");
    store.ingest(span("doomed", "s2", json!({}))).expect("s2");
    store
        .annotate(
            serde_json::from_value(json!({
                "trace_id": "doomed", "span_id": "s1", "name": "quality", "value": 1,
            }))
            .expect("annotation"),
        )
        .expect("annotates doomed");
    store
        .annotate(
            serde_json::from_value(json!({
                "trace_id": "kept", "span_id": "k1", "name": "quality", "value": 2,
            }))
            .expect("annotation"),
        )
        .expect("annotates kept");

    let status = store
        .erase(Subject::Trace {
            trace_id: "doomed".into(),
        })
        .expect("erases");
    let settle = status.settle.expect("the erasure settles synchronously");
    assert_eq!(settle.spans_removed, 2, "buffered and sealed spans both go");
    assert_eq!(settle.annotations_removed, 1);

    assert!(store.get_trace("doomed").expect("lookup").is_empty());
    assert_eq!(store.get_trace("kept").expect("lookup").len(), 1);
    assert!(
        store
            .query(&SpanFilter::default())
            .expect("query")
            .iter()
            .all(|span| span.trace_id != "doomed"),
        "no query surface returns the erased trace"
    );
    assert!(store
        .annotations("doomed", None, None)
        .expect("ann")
        .is_empty());
    assert_eq!(store.annotations("kept", None, None).expect("ann").len(), 1);
    for segment in store.persisted_segment_spans().expect("segments") {
        assert!(segment.iter().all(|span| span.trace_id != "doomed"));
    }

    let receipt = store.verify_erasure(status.erase.id).expect("receipt");
    assert_eq!(
        receipt.result,
        "erased",
        "receipt:\n{}",
        receipt.render_text()
    );
    assert!(receipt.settled);
    assert_eq!(
        domain(&receipt, "tombstone-log").result,
        "retained-by-design",
        "the record of the erasure is named, not hidden"
    );
}

#[test]
fn a_published_erasure_survives_reopen_without_the_log_resurrecting_it() {
    let dir = test_dir("reopen");
    {
        let store = Store::open(&dir, wal_config()).expect("opens");
        // Never flushed: these spans live in the buffer and the write-ahead
        // log only, which is exactly where a bad deletion gets undone by the
        // next restart's replay.
        store.ingest(span("doomed", "s1", json!({}))).expect("s1");
        store.ingest(span("kept", "k1", json!({}))).expect("k1");
        store
            .erase(Subject::Trace {
                trace_id: "doomed".into(),
            })
            .expect("erases");
    }
    let store = Store::open(&dir, wal_config()).expect("reopens");
    assert!(
        store.get_trace("doomed").expect("lookup").is_empty(),
        "replay must not resurrect an erased span"
    );
    assert_eq!(
        store.get_trace("kept").expect("lookup").len(),
        1,
        "the surviving acknowledged span is still here"
    );
    let erasures = store.erasures().expect("list");
    assert_eq!(erasures.len(), 1);
    assert!(erasures[0].settle.is_some(), "the settle record replayed");
}

#[test]
fn superseded_versions_of_an_erased_key_purge_with_it() {
    let dir = test_dir("superseded");
    let store = Store::open(&dir, wal_config()).expect("opens");
    store
        .ingest(span("t", "s", json!({"version": "old"})))
        .expect("v1");
    store.flush().expect("seals v1");
    store
        .ingest(span("t", "s", json!({"version": "new"})))
        .expect("v2");
    store.flush().expect("seals v2");

    let status = store
        .erase(Subject::Span {
            trace_id: "t".into(),
            span_id: "s".into(),
        })
        .expect("erases");
    assert_eq!(
        status.settle.expect("settled").spans_removed,
        2,
        "both physical versions held the bytes, so both count and both go"
    );
    for segment in store.persisted_segment_spans().expect("segments") {
        assert!(segment.is_empty(), "no version survives in any segment");
    }
}

#[test]
fn session_erasure_resolves_across_mixed_conventions() {
    let dir = test_dir("session");
    let store = Store::open(&dir, wal_config()).expect("opens");
    store
        .ingest(span("t1", "a", json!({"session.id": "sess-9"})))
        .expect("native");
    store
        .ingest(span("t2", "b", json!({"gen_ai.conversation.id": "sess-9"})))
        .expect("genai");
    store
        .ingest(span("t3", "c", json!({"session.id": "sess-other"})))
        .expect("other");
    store.flush().expect("seals");

    let status = store
        .erase(Subject::Session {
            session_id: "sess-9".into(),
        })
        .expect("erases");
    assert_eq!(status.settle.expect("settled").spans_removed, 2);
    assert!(
        store.session("sess-9").expect("session").is_none(),
        "the session no longer resolves"
    );
    assert!(
        store.session("sess-other").expect("session").is_some(),
        "an unrelated session is untouched"
    );
    assert_eq!(
        store
            .verify_erasure(status.erase.id)
            .expect("receipt")
            .result,
        "erased"
    );
}

#[test]
fn payload_erasure_deletes_the_bytes_and_redacts_every_preview() {
    let dir = test_dir("payload");
    let store = Store::open(
        &dir,
        Config {
            payload_threshold: Some(64),
            ..wal_config()
        },
    )
    .expect("opens");

    let secret = format!("confidential {}", "x".repeat(200));
    store
        .ingest(span("t1", "a", json!({"prompt": secret})))
        .expect("first reference");
    store
        .ingest(span("t2", "b", json!({"prompt": secret})))
        .expect("second reference, deduplicated");
    store.flush().expect("seals");

    // The reference is content-addressed, so it is derivable from the text.
    let reference = format!("sha256/{}", traza::payload::sha256_hex(secret.as_bytes()));
    assert!(
        store.payload(&reference).expect("load").is_some(),
        "the payload file exists before the erasure"
    );
    assert_eq!(
        store
            .query(&SpanFilter {
                content: Some("confidential".into()),
                ..SpanFilter::default()
            })
            .expect("content search")
            .len(),
        2,
        "the inline previews are searchable before the erasure"
    );

    let status = store
        .erase(Subject::Payload {
            reference: reference.clone(),
        })
        .expect("erases");
    let settle = status.settle.expect("settled");
    assert_eq!(
        settle.spans_redacted, 2,
        "every referencing span is rewritten"
    );
    assert_eq!(
        settle.spans_removed, 0,
        "redaction removes values, not spans"
    );
    assert_eq!(settle.payloads_removed, vec![reference.clone()]);

    assert!(
        store.payload(&reference).expect("load").is_none(),
        "the payload bytes are gone"
    );
    for trace in ["t1", "t2"] {
        let spans = store.get_trace(trace).expect("lookup");
        assert_eq!(spans.len(), 1, "the span itself survives");
        let value = &spans[0].attributes["prompt"];
        assert_eq!(value.get("erased"), Some(&Value::Bool(true)));
        assert!(
            value.get("preview").is_none(),
            "the preview is content and goes with the content"
        );
    }
    assert!(
        store
            .query(&SpanFilter {
                content: Some("confidential".into()),
                ..SpanFilter::default()
            })
            .expect("content search")
            .is_empty(),
        "the rewritten segments' content index no longer knows the preview text"
    );
    assert_eq!(
        store
            .verify_erasure(status.erase.id)
            .expect("receipt")
            .result,
        "erased"
    );
}

#[test]
fn trace_erasure_is_reference_aware_about_shared_payloads() {
    let dir = test_dir("shared-payload");
    let store = Store::open(
        &dir,
        Config {
            payload_threshold: Some(64),
            ..wal_config()
        },
    )
    .expect("opens");
    let shared = format!("shared {}", "y".repeat(200));
    let reference = format!("sha256/{}", traza::payload::sha256_hex(shared.as_bytes()));
    store
        .ingest(span("t1", "a", json!({"prompt": shared})))
        .expect("t1");
    store
        .ingest(span("t2", "b", json!({"prompt": shared})))
        .expect("t2");
    store.flush().expect("seals");

    // Erasing t1 must not destroy bytes t2 still references.
    let first = store
        .erase(Subject::Trace {
            trace_id: "t1".into(),
        })
        .expect("erases t1");
    let settle = first.settle.expect("settled");
    assert!(settle.payloads_removed.is_empty());
    assert_eq!(settle.payloads_retained.len(), 1);
    assert!(settle.payloads_retained[0]
        .reason
        .contains("still referenced"));
    assert!(
        store.payload(&reference).expect("load").is_some(),
        "the shared bytes survive the first erasure"
    );
    let receipt = store.verify_erasure(first.erase.id).expect("receipt");
    assert_eq!(
        receipt.result,
        "erased",
        "a retention with a stated reason is not a failure:\n{}",
        receipt.render_text()
    );
    assert!(
        domain(&receipt, "payloads").items[0].contains("retained"),
        "the receipt names the retention and its reason"
    );

    // Erasing the second trace removes the last reference, and the bytes.
    let second = store
        .erase(Subject::Trace {
            trace_id: "t2".into(),
        })
        .expect("erases t2");
    assert_eq!(
        second.settle.expect("settled").payloads_removed,
        vec![reference.clone()]
    );
    assert!(store.payload(&reference).expect("load").is_none());
}

#[test]
fn a_pending_tombstone_masks_at_open_and_resume_settles_it() {
    let dir = test_dir("pending");
    {
        let store = Store::open(&dir, wal_config()).expect("opens");
        store.ingest(span("txp", "s1", json!({}))).expect("s1");
        store.ingest(span("kept", "k1", json!({}))).expect("k1");
        store.flush().expect("seals");
    }
    // Plant the crash: an erase record with no settle, exactly what a
    // process killed between the tombstone fsync and the purge leaves.
    let line = json!({
        "op": "erase", "schema": 1, "id": 1, "requested_unix_ns": 123,
        "subject": {"kind": "trace", "trace_id": "txp"},
        "span_keys": [["txp", "s1"]], "payload_refs": [],
    });
    let mut log = String::new();
    log.push_str(&line.to_string());
    log.push('\n');
    std::fs::write(dir.join("tombstones.jsonl"), log).expect("plants tombstone");

    let store = Store::open(&dir, wal_config()).expect("reopens");
    assert!(
        store.get_trace("txp").expect("lookup").is_empty(),
        "a pending erasure masks its subject before any purge runs"
    );
    assert!(
        store
            .query(&SpanFilter::default())
            .expect("query")
            .iter()
            .all(|span| span.trace_id != "txp"),
        "the mask holds on the search path too"
    );
    // The bytes are still physically present — the purge has not run — and
    // the receipt says so rather than taking the mask's word for it.
    assert_eq!(
        store.verify_erasure(1).expect("receipt").result,
        "incomplete",
        "an unsettled erasure never verifies as erased"
    );

    assert_eq!(store.resume_erasures().expect("resumes"), 1);
    assert_eq!(store.resume_erasures().expect("idempotent"), 0);
    assert_eq!(store.verify_erasure(1).expect("receipt").result, "erased");
    assert_eq!(store.get_trace("kept").expect("lookup").len(), 1);
}

#[test]
fn the_live_tail_stops_serving_erased_spans_but_serves_new_ones() {
    let dir = test_dir("tail");
    let store = Store::open(&dir, wal_config()).expect("opens");
    store.ingest(span("doomed", "s1", json!({}))).expect("s1");
    store.ingest(span("kept", "k1", json!({}))).expect("k1");

    store
        .erase(Subject::Trace {
            trace_id: "doomed".into(),
        })
        .expect("erases");
    let read = store
        .tail_after(None, 64, 64, &SpanFilter::default(), Duration::ZERO)
        .expect("tail");
    match read {
        traza::tail::TailRead::Batch { spans, .. } => {
            assert!(
                spans.iter().all(|span| span.trace_id != "doomed"),
                "the ring must not serve what the store no longer holds"
            );
            assert!(spans.iter().any(|span| span.trace_id == "kept"));
        }
        other => panic!("expected a batch, got {other:?}"),
    }

    // A barrier, not a ban: the same identifiers ingested after the erasure
    // settled are new data, and the tail serves them.
    store
        .ingest(span("doomed", "s1", json!({})))
        .expect("re-ingest");
    let read = store
        .tail_after(None, 64, 64, &SpanFilter::default(), Duration::ZERO)
        .expect("tail");
    match read {
        traza::tail::TailRead::Batch { spans, .. } => {
            assert!(
                spans
                    .iter()
                    .any(|span| span.trace_id == "doomed" && span.span_id == "s1"),
                "post-settle admissions flow normally"
            );
        }
        other => panic!("expected a batch, got {other:?}"),
    }
}

#[test]
fn re_delivered_data_fails_the_receipt_and_new_activity_does_not() {
    let dir = test_dir("redelivery");
    let store = Store::open(&dir, wal_config()).expect("opens");
    store.ingest(span("t", "old", json!({}))).expect("old");
    store.flush().expect("seals");
    let status = store
        .erase(Subject::Trace {
            trace_id: "t".into(),
        })
        .expect("erases");
    let erased = keys_of(&[span("t", "old", json!({}))]);
    assert_eq!(
        keys_of(
            &status
                .erase
                .span_keys
                .iter()
                .map(|(trace, id)| span(trace, id, json!({})))
                .collect::<Vec<_>>()
        ),
        erased,
        "the record carries the resolved keys"
    );

    // New activity under the erased trace id: reported, never a failure.
    store.ingest(span("t", "new", json!({}))).expect("new key");
    let receipt = store.verify_erasure(status.erase.id).expect("receipt");
    assert_eq!(
        receipt.result,
        "erased",
        "an erasure is a barrier, not a ban:\n{}",
        receipt.render_text()
    );
    assert_eq!(domain(&receipt, "write-buffer").new_activity, 1);

    // Re-delivery of an erased key: the receipt must refuse.
    store
        .ingest(span("t", "old", json!({})))
        .expect("re-delivery");
    let receipt = store.verify_erasure(status.erase.id).expect("receipt");
    assert_eq!(receipt.result, "incomplete");
    assert_eq!(domain(&receipt, "write-buffer").re_delivered, 1);
}

#[test]
fn a_pin_holding_the_subject_flips_the_receipt_until_released() {
    let dir = test_dir("pins");
    let store = Store::open(&dir, wal_config()).expect("opens");
    store.ingest(span("doomed", "s1", json!({}))).expect("s1");
    store.flush().expect("seals");
    store.pin_generation("pre-erase").expect("pins");

    let status = store
        .erase(Subject::Trace {
            trace_id: "doomed".into(),
        })
        .expect("erases");
    let receipt = store.verify_erasure(status.erase.id).expect("receipt");
    assert_eq!(
        receipt.result, "incomplete",
        "a hard-linked copy of the subject is not erased, and the receipt says so"
    );
    assert!(domain(&receipt, "pins").result == "holds-data");
    assert!(domain(&receipt, "pins").items[0].contains("pre-erase"));

    store.release_pin("pre-erase").expect("releases");
    assert_eq!(
        store
            .verify_erasure(status.erase.id)
            .expect("receipt")
            .result,
        "erased"
    );
}

// ---------------------------------------------------------------- process

struct Server {
    child: Child,
    port: u16,
}

/// Every spawned server dies with its handle. The restarted server in the
/// kill test outlives its `Server` otherwise, and a leaked child holds the
/// test binary's output pipe open — which stalls the whole `cargo test`
/// invocation waiting for an EOF that never comes.
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    fn spawn(data_dir: &Path) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_traza-server"));
        let mut child = command
            .arg("--data-dir")
            .arg(data_dir)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg("0")
            .arg("--durability")
            .arg("wal")
            // The point is what survives WITHOUT a segment flush.
            .arg("--flush-spans")
            .arg("1000000")
            .env_remove("TRAZA_TOKENS")
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

    /// SIGKILL: no unwinding, no flush on the way out. Idempotent with the
    /// `Drop` kill that follows.
    fn kill_hard(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn request(&self, method: &str, target: &str, body: Option<&Value>) -> (u16, Value) {
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
        write!(
            stream,
            "{method} {target} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\n\
             Content-Length: {length}\r\nConnection: close\r\n\r\n"
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

#[test]
fn an_acknowledged_erasure_survives_kill_dash_nine() {
    let dir = test_dir("kill");
    let mut server = Server::spawn(&dir);

    let (status, _) = server.request(
        "POST",
        "/v1/spans",
        Some(&json!([
            {"trace_id": "doomed", "span_id": "s1", "name": "n", "service": "svc",
             "start_time_unix_nano": 1_000u64, "end_time_unix_nano": 2_000u64},
            {"trace_id": "kept", "span_id": "k1", "name": "n", "service": "svc",
             "start_time_unix_nano": 1_000u64, "end_time_unix_nano": 2_000u64},
        ])),
    );
    assert_eq!(status, 200);

    let (status, settled) = server.request(
        "POST",
        "/v1/erasures",
        Some(&json!({"subject": {"kind": "trace", "trace_id": "doomed"}})),
    );
    assert_eq!(status, 200, "the erasure acknowledges only once settled");
    assert_eq!(settled["settle"]["spans_removed"], json!(1));

    // The 200 is the acknowledgement, and the acknowledgement is the claim
    // under test: nothing after it may be load-bearing.
    server.kill_hard();

    let server = Server::spawn(&dir);
    let (status, spans) = server.request("GET", "/v1/spans", None);
    assert_eq!(status, 200);
    let spans = spans["spans"].as_array().expect("spans array").clone();
    assert!(
        spans.iter().all(|span| span["trace_id"] != json!("doomed")),
        "an acknowledged erasure must hold across kill -9: {spans:?}"
    );
    assert!(spans.iter().any(|span| span["trace_id"] == json!("kept")));

    let (status, receipt) = server.request("GET", "/v1/erasures/1/verify", None);
    assert_eq!(status, 200);
    assert_eq!(
        receipt["result"],
        json!("erased"),
        "the receipt verifies after the restart: {receipt}"
    );
}
