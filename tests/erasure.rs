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
            tenant: String::new(),
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
    assert!(
        receipt.conclusive,
        "no occurrence scan has anything to point at:\n{}",
        receipt.render_text()
    );
    assert_eq!(
        domain(&receipt, "tombstone-log").result,
        "retained-by-design",
        "the record of the erasure is named, not hidden"
    );
}

#[test]
fn spans_covered_by_a_pending_erasure_are_suppressed_at_admission() {
    let dir = test_dir("suppression");
    {
        let store = Store::open(&dir, wal_config()).expect("opens");
        store.ingest(span("txp", "s1", json!({}))).expect("s1");
        store.flush().expect("seals");
    }
    // The crash state: intent recorded, purge never ran.
    let line = json!({
        "op": "erase", "schema": 1, "id": 1, "requested_unix_ns": 123,
        "subject": {"kind": "trace", "trace_id": "txp"},
        "span_keys": [["txp", "s1"]], "payload_refs": [],
    });
    std::fs::write(dir.join("tombstones.jsonl"), format!("{line}\n")).expect("plants");

    let store = Store::open(&dir, wal_config()).expect("reopens");
    // Admission is the barrier: a covered span sent while the erasure is
    // pending is dropped BEFORE the log carries it, not stored-and-hidden —
    // and the admission says so rather than counting it as stored.
    let admission = store.ingest(span("txp", "s2", json!({}))).expect("acked");
    assert_eq!(
        (admission.accepted, admission.suppressed),
        (0, 1),
        "a suppressed span is never reported as accepted"
    );
    let admission = store.ingest(span("other", "o1", json!({}))).expect("other");
    assert_eq!((admission.accepted, admission.suppressed), (1, 0));
    assert_eq!(store.resume_erasures().expect("settles"), 1);
    assert!(
        store.get_trace("txp").expect("lookup").is_empty(),
        "neither the original span nor the suppressed one survives the settle"
    );
    assert_eq!(store.get_trace("other").expect("lookup").len(), 1);
    // After settle the barrier lifts: the same identifiers are new data.
    store
        .ingest(span("txp", "s2", json!({})))
        .expect("new data");
    assert_eq!(store.get_trace("txp").expect("lookup").len(), 1);
}

#[test]
fn a_suppressed_span_leaves_no_payload_bytes_behind() {
    let dir = test_dir("suppressed-payload");
    let secret = format!("orphan {}", "o".repeat(200));
    let reference = format!("sha256/{}", traza::payload::sha256_hex(secret.as_bytes()));
    {
        let store = Store::open(
            &dir,
            Config {
                payload_threshold: Some(64),
                ..wal_config()
            },
        )
        .expect("opens");
        store.ingest(span("txp", "s1", json!({}))).expect("s1");
        store.flush().expect("seals");
    }
    let line = json!({
        "op": "erase", "schema": 1, "id": 1, "requested_unix_ns": 123,
        "subject": {"kind": "trace", "trace_id": "txp"},
        "span_keys": [["txp", "s1"]], "payload_refs": [],
    });
    std::fs::write(dir.join("tombstones.jsonl"), format!("{line}\n")).expect("plants");

    let store = Store::open(
        &dir,
        Config {
            payload_threshold: Some(64),
            ..wal_config()
        },
    )
    .expect("reopens");
    // The covered span carries an oversized value. Suppression must bar the
    // OFFLOAD too: a span dropped after its payload was written would leave
    // orphan bytes of the erased subject that no record names — invisible to
    // the purge, the receipt, and `payload_refs`, because the span that
    // carried them never entered the store.
    let admission = store
        .ingest(span("txp", "s2", json!({"prompt": secret})))
        .expect("acked");
    assert_eq!((admission.accepted, admission.suppressed), (0, 1));
    assert!(
        store.payload(&reference).expect("load").is_none(),
        "no payload bytes while pending"
    );
    let hash = reference.strip_prefix("sha256/").expect("hash");
    let path = dir
        .join("payloads")
        .join(&hash[..2])
        .join(format!("{hash}.bin"));
    assert!(
        !path.exists(),
        "the suppressed span's payload must never reach the filesystem"
    );

    assert_eq!(store.resume_erasures().expect("settles"), 1);
    assert!(!path.exists(), "and it is not resurrected by the settle");
    assert!(store.payload(&reference).expect("load").is_none());
    let receipt = store.verify_erasure(1).expect("receipt");
    assert_eq!(receipt.result, "erased", "{}", receipt.render_text());
}

#[test]
fn offloading_content_under_a_pending_payload_erasure_writes_a_marker_not_bytes() {
    let dir = test_dir("offload-masked");
    let config = Config {
        payload_threshold: Some(64),
        ..wal_config()
    };
    let secret = format!("masked {}", "m".repeat(200));
    let reference = format!("sha256/{}", traza::payload::sha256_hex(secret.as_bytes()));
    {
        let store = Store::open(&dir, config.clone()).expect("opens");
        store
            .ingest(span("t1", "a", json!({"prompt": secret})))
            .expect("ingests");
        store.flush().expect("seals");
    }
    // A pending PAYLOAD erasure, as a crash would leave it.
    let line = json!({
        "op": "erase", "schema": 1, "id": 1, "requested_unix_ns": 123,
        "subject": {"kind": "payload", "reference": reference},
        "span_keys": [["t1", "a"]], "payload_refs": [reference],
    });
    std::fs::write(dir.join("tombstones.jsonl"), format!("{line}\n")).expect("plants");

    let store = Store::open(&dir, config).expect("reopens");
    // A NEW span carrying the doomed content is new data — the span is
    // admitted — but its oversized value must not recreate the file the
    // erasure is deleting: it offloads directly to the redacted marker.
    let admission = store
        .ingest(span("t2", "b", json!({"prompt": secret})))
        .expect("acked");
    assert_eq!((admission.accepted, admission.suppressed), (1, 0));

    assert_eq!(store.resume_erasures().expect("settles"), 1);
    assert!(
        store.payload(&reference).expect("load").is_none(),
        "the file is gone and was not recreated by the mid-erasure ingest"
    );
    let spans = store.get_trace("t2").expect("lookup");
    assert_eq!(spans.len(), 1, "the new span survives the erasure");
    let value = &spans[0].attributes["prompt"];
    assert_eq!(value.get("erased"), Some(&Value::Bool(true)));
    assert!(value.get("preview").is_none());
    let receipt = store.verify_erasure(1).expect("receipt");
    assert_eq!(receipt.result, "erased", "{}", receipt.render_text());
}

#[test]
fn the_settle_names_a_generation_the_erasure_does_not_then_invalidate() {
    let dir = test_dir("generation-integrity");
    let store = Store::open(&dir, wal_config()).expect("opens");
    store.ingest(span("doomed", "s1", json!({}))).expect("s1");
    store.ingest(span("kept", "k1", json!({}))).expect("k1");
    store
        .annotate(
            serde_json::from_value(json!({
                "trace_id": "doomed", "span_id": "s1", "name": "quality", "value": 1,
            }))
            .expect("annotation"),
        )
        .expect("annotates");
    store.flush().expect("seals");

    let status = store
        .erase(Subject::Trace {
            tenant: String::new(),
            trace_id: "doomed".into(),
        })
        .expect("erases");
    let generation = status.settle.expect("settled").generation;
    // Every rewrite the erasure performs — segments, the annotation log —
    // happens BEFORE the checkpoint the settle record cites, so the cited
    // generation must verify clean. (The settle append itself rides the
    // append-only allowance the manifest grants the tombstone log.)
    assert!(
        store
            .verify_generation(generation)
            .expect("verifies")
            .is_empty(),
        "the settle's generation digests the store the erasure left behind"
    );
}

#[test]
fn annotations_addressed_to_a_pending_subject_are_suppressed_at_admission() {
    let dir = test_dir("annotation-barrier");
    {
        let store = Store::open(&dir, wal_config()).expect("opens");
        store.ingest(span("txp", "s1", json!({}))).expect("s1");
        store.flush().expect("seals");
    }
    let line = json!({
        "op": "erase", "schema": 1, "id": 1, "requested_unix_ns": 123,
        "subject": {"kind": "trace", "trace_id": "txp"},
        "span_keys": [["txp", "s1"]], "payload_refs": [],
    });
    std::fs::write(dir.join("tombstones.jsonl"), format!("{line}\n")).expect("plants");

    let store = Store::open(&dir, wal_config()).expect("reopens");
    // An annotation landing between the erasure's annotation drop and its
    // settle would attach judgment to erased data; the barrier drops it
    // exactly as it drops the subject's spans.
    store
        .annotate(
            serde_json::from_value(json!({
                "trace_id": "txp", "span_id": "s1", "name": "late", "value": 1,
            }))
            .expect("annotation"),
        )
        .expect("acknowledged");
    store
        .annotate(
            serde_json::from_value(json!({
                "trace_id": "other", "span_id": "o1", "name": "kept", "value": 2,
            }))
            .expect("annotation"),
        )
        .expect("stored");
    assert_eq!(store.resume_erasures().expect("settles"), 1);
    assert!(
        store
            .annotations("txp", None, None)
            .expect("ann")
            .is_empty(),
        "the covered annotation was never stored"
    );
    assert_eq!(
        store.annotations("other", None, None).expect("ann").len(),
        1,
        "an uncovered annotation flows normally through the barrier"
    );
}

#[test]
fn the_begin_transition_leaves_no_orphan_bytes_behind() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    // The steady pending state is the easy half; the holes review kept
    // finding lived at the TRANSITION — a batch or annotation astride
    // `begin`, its mask loaded before the erasure existed and its side
    // effects landing after. This test aims writers with unique oversized
    // payloads and annotators at the subject while erasures fire
    // repeatedly, then audits the filesystem itself. Before the erasure
    // gate, the offload-then-suppress race left payload files no record
    // named — precisely what the final sweep below hunts.
    let dir = test_dir("begin-transition");
    let store = Arc::new(
        Store::open(
            &dir,
            Config {
                payload_threshold: Some(64),
                ..wal_config()
            },
        )
        .expect("opens"),
    );
    let stop = Arc::new(AtomicBool::new(false));
    let mut workers = Vec::new();
    for thread in 0..3 {
        let store = Arc::clone(&store);
        let stop = Arc::clone(&stop);
        workers.push(std::thread::spawn(move || {
            let mut index = 0usize;
            while !stop.load(Ordering::Relaxed) {
                let id = format!("w-{thread}-{index}");
                index += 1;
                // Unique content per span: no legitimate sharing, so any
                // file on disk without a referencing span is an orphan.
                let text = format!("payload {thread} {index} {}", "x".repeat(150));
                store
                    .ingest(span("doomed", &id, json!({"prompt": text})))
                    .expect("ingest never errors");
                store
                    .annotate(
                        serde_json::from_value(json!({
                            "trace_id": "doomed", "span_id": id, "name": "note", "value": 1,
                        }))
                        .expect("annotation"),
                    )
                    .expect("annotate never errors");
            }
        }));
    }

    for _ in 0..8 {
        std::thread::sleep(Duration::from_millis(10));
        let status = store
            .erase(Subject::Trace {
                tenant: String::new(),
                trace_id: "doomed".into(),
            })
            .expect("erases");
        let settle = status.settle.expect("settled");
        let problems = store
            .verify_generation(settle.generation)
            .expect("verifies");
        assert!(
            problems.is_empty(),
            "every cycle's cited generation verifies, transitions included: {problems:?}"
        );
    }
    stop.store(true, Ordering::Relaxed);
    for worker in workers {
        worker.join().expect("worker");
    }

    // One quiescent erasure closes the run, so everything the concurrent
    // cycles legitimately admitted after their settles is erased too — and
    // then the store must hold NOTHING of the subject, in any domain, on
    // disk or off.
    store
        .erase(Subject::Trace {
            tenant: String::new(),
            trace_id: "doomed".into(),
        })
        .expect("final erase");
    assert!(store.get_trace("doomed").expect("lookup").is_empty());
    assert!(store
        .annotations("doomed", None, None)
        .expect("ann")
        .is_empty());
    assert!(store
        .search_annotations(&traza::annotations::AnnotationQuery::default())
        .expect("search")
        .is_empty());

    // The filesystem audit: every payload was unique to a "doomed" span, so
    // a single surviving file is an orphan some race left behind.
    let payloads = dir.join("payloads");
    let mut leftover: Vec<String> = Vec::new();
    if payloads.exists() {
        for shard in std::fs::read_dir(&payloads).expect("payloads dir") {
            let shard = shard.expect("shard");
            if !shard.file_type().expect("type").is_dir() {
                continue;
            }
            for file in std::fs::read_dir(shard.path()).expect("shard dir") {
                leftover.push(file.expect("file").path().display().to_string());
            }
        }
    }
    assert!(
        leftover.is_empty(),
        "orphan payload bytes survived the erasures: {leftover:?}"
    );
}

#[test]
fn no_span_acknowledged_before_settle_survives_a_concurrent_erasure() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    // The reviewer's probe, kept as a regression test: writers hammer the
    // doomed trace while the erasure runs. The erasure's contract is a cut
    // at settle time — every span acknowledged before `settled_unix_ns` is
    // erased or was never stored, and every survivor was acknowledged after.
    let dir = test_dir("concurrent");
    let store = Arc::new(Store::open(&dir, wal_config()).expect("opens"));
    for index in 0..20 {
        store
            .ingest(span("doomed", &format!("pre-{index}"), json!({})))
            .expect("pre");
    }
    store.flush().expect("seals");

    let stop = Arc::new(AtomicBool::new(false));
    let mut writers = Vec::new();
    for thread in 0..4 {
        let store = Arc::clone(&store);
        let stop = Arc::clone(&stop);
        writers.push(std::thread::spawn(move || {
            let mut acked: Vec<(String, u64)> = Vec::new();
            let mut index = 0;
            while !stop.load(Ordering::Relaxed) {
                let id = format!("w-{thread}-{index}");
                index += 1;
                store
                    .ingest(span("doomed", &id, json!({})))
                    .expect("ingest never errors");
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos() as u64;
                acked.push((id, now));
            }
            acked
        }));
    }

    std::thread::sleep(Duration::from_millis(20));
    let status = store
        .erase(Subject::Trace {
            tenant: String::new(),
            trace_id: "doomed".into(),
        })
        .expect("erases");
    let settle = status.settle.expect("settled");
    let settled_unix_ns = settle.settled_unix_ns;
    std::thread::sleep(Duration::from_millis(20));
    stop.store(true, Ordering::Relaxed);
    let mut acked: Vec<(String, u64)> = Vec::new();
    for writer in writers {
        acked.extend(writer.join().expect("writer"));
    }

    let survivors: HashSet<String> = store
        .get_trace("doomed")
        .expect("lookup")
        .into_iter()
        .map(|span| span.span_id)
        .collect();
    assert!(
        !survivors.iter().any(|id| id.starts_with("pre-")),
        "nothing from before the erasure survives"
    );
    for (id, acked_ns) in &acked {
        if survivors.contains(id) {
            assert!(
                acked_ns >= &settled_unix_ns,
                "span {id} was acknowledged {acked_ns} — before settle at \
                 {settled_unix_ns} — and survived; the cut leaked"
            );
        }
    }
    let receipt = store.verify_erasure(status.erase.id).expect("receipt");
    assert_eq!(
        receipt.result,
        "erased",
        "survivors are post-settle new activity, never re-deliveries:\n{}",
        receipt.render_text()
    );
    assert!(
        store
            .verify_generation(settle.generation)
            .expect("verifies")
            .is_empty(),
        "the cited generation stays intact even under concurrent writers"
    );
}

#[test]
fn a_pin_taken_before_an_erasure_is_not_edited_by_it() {
    let dir = test_dir("pin-immutable");
    let store = Store::open(&dir, wal_config()).expect("opens");
    store.ingest(span("first", "a", json!({}))).expect("first");
    store
        .ingest(span("second", "b", json!({})))
        .expect("second");
    store.flush().expect("seals");
    // Erasure #1 creates the tombstone log, so the pin below carries it.
    store
        .erase(Subject::Trace {
            tenant: String::new(),
            trace_id: "first".into(),
        })
        .expect("erasure one");
    store.pin_generation("before-two").expect("pins");
    let pinned_log = store.pin_path("before-two").join("tombstones.jsonl");
    let pinned_len = std::fs::metadata(&pinned_log).expect("pinned log").len();

    store
        .erase(Subject::Trace {
            tenant: String::new(),
            trace_id: "second".into(),
        })
        .expect("erasure two");

    // The live log grew; the pinned copy did not — no shared inode, no
    // retroactive edits to a backup.
    assert_eq!(
        std::fs::metadata(&pinned_log).expect("pinned log").len(),
        pinned_len,
        "an erasure after the pin must not append through the pinned file"
    );
    let pinned_bytes = std::fs::read(&pinned_log).expect("read");
    assert!(
        !String::from_utf8_lossy(&pinned_bytes).contains("\"id\":2"),
        "erasure #2 must not appear in a pin taken before it"
    );
    assert!(
        store.verify_pin("before-two").expect("verify").is_empty(),
        "the prefix copy verifies against the pin's manifest"
    );

    // Restoring that pin yields the consistent point-in-time state: the
    // second trace present, and NO record claiming it was erased.
    let restored_dir = test_dir("pin-immutable-restored");
    let restored = Store::restore(&restored_dir, store.pin_path("before-two"), wal_config())
        .expect("restores");
    assert_eq!(
        restored.get_trace("second").expect("lookup").len(),
        1,
        "the pinned state predates erasure #2"
    );
    let erasures = restored.erasures().expect("list");
    assert_eq!(erasures.len(), 1, "only erasure #1 exists in the pin");
    assert!(erasures[0].settle.is_some());
}

#[test]
fn a_pending_erasure_masks_annotations_and_payload_bytes_too() {
    let dir = test_dir("mask-satellites");
    let secret = format!("withheld {}", "z".repeat(200));
    let reference = format!("sha256/{}", traza::payload::sha256_hex(secret.as_bytes()));
    {
        let store = Store::open(
            &dir,
            Config {
                payload_threshold: Some(64),
                ..wal_config()
            },
        )
        .expect("opens");
        store
            .ingest(span("txp", "s1", json!({"prompt": secret})))
            .expect("s1");
        store
            .annotate(
                serde_json::from_value(json!({
                    "trace_id": "txp", "span_id": "s1", "name": "quality", "value": 1,
                }))
                .expect("annotation"),
            )
            .expect("annotates");
        store.flush().expect("seals");
    }
    let line = json!({
        "op": "erase", "schema": 1, "id": 1, "requested_unix_ns": 123,
        "subject": {"kind": "trace", "trace_id": "txp"},
        "span_keys": [["txp", "s1"]], "payload_refs": [reference],
    });
    std::fs::write(dir.join("tombstones.jsonl"), format!("{line}\n")).expect("plants");

    let store = Store::open(
        &dir,
        Config {
            payload_threshold: Some(64),
            ..wal_config()
        },
    )
    .expect("reopens");
    assert!(
        store
            .annotations("txp", None, None)
            .expect("ann")
            .is_empty(),
        "annotations about a pending subject are withheld"
    );
    assert!(
        store
            .search_annotations(&traza::annotations::AnnotationQuery::default())
            .expect("search")
            .is_empty(),
        "the cross-trace search withholds them too"
    );
    assert!(
        store.payload(&reference).expect("load").is_none(),
        "payload bytes a pending erasure must account for are withheld"
    );
}

#[test]
fn an_uppercase_payload_reference_is_canonicalized_not_mismatched() {
    let dir = test_dir("uppercase");
    let store = Store::open(
        &dir,
        Config {
            payload_threshold: Some(64),
            ..wal_config()
        },
    )
    .expect("opens");
    let secret = format!("cased {}", "q".repeat(200));
    let reference = format!("sha256/{}", traza::payload::sha256_hex(secret.as_bytes()));
    store
        .ingest(span("t1", "a", json!({"prompt": secret})))
        .expect("ingests");
    store.flush().expect("seals");

    let uppercase = reference.replace("sha256/", "").to_ascii_uppercase();
    let status = store
        .erase(Subject::Payload {
            reference: format!("sha256/{uppercase}"),
        })
        .expect("erases");
    assert_eq!(
        status.erase.subject,
        Subject::Payload {
            reference: reference.clone()
        },
        "the recorded subject is the canonical lowercase form"
    );
    assert_eq!(status.settle.expect("settled").spans_redacted, 1);
    assert!(store.payload(&reference).expect("load").is_none());
    let spans = store.get_trace("t1").expect("lookup");
    assert!(
        spans[0].attributes["prompt"].get("preview").is_none(),
        "the preview was redacted — an uppercase request must not miss it"
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
fn a_recreated_payload_file_fails_the_receipt_instead_of_reading_as_retained() {
    let dir = test_dir("recreated");
    let store = Store::open(
        &dir,
        Config {
            payload_threshold: Some(64),
            ..wal_config()
        },
    )
    .expect("opens");
    let secret = format!("returning {}", "r".repeat(200));
    let reference = format!("sha256/{}", traza::payload::sha256_hex(secret.as_bytes()));
    store
        .ingest(span("t1", "a", json!({"prompt": secret})))
        .expect("ingests");
    store.flush().expect("seals");
    let status = store
        .erase(Subject::Payload {
            reference: reference.clone(),
        })
        .expect("erases");
    assert_eq!(
        store
            .verify_erasure(status.erase.id)
            .expect("receipt")
            .result,
        "erased"
    );

    // Put the bytes back by hand — a re-delivery outside any span. The only
    // things pointing at this content are redaction markers, and a marker is
    // the record that content is GONE; it must not read as a live reference
    // that certifies the file as safely retained.
    let hash = reference.strip_prefix("sha256/").expect("hash");
    let path = dir
        .join("payloads")
        .join(&hash[..2])
        .join(format!("{hash}.bin"));
    std::fs::create_dir_all(path.parent().expect("shard")).expect("dirs");
    std::fs::write(&path, secret.as_bytes()).expect("recreates");

    let receipt = store.verify_erasure(status.erase.id).expect("receipt");
    assert_eq!(
        receipt.result,
        "incomplete",
        "recreated bytes with no live referent must fail:\n{}",
        receipt.render_text()
    );
    assert!(domain(&receipt, "payloads")
        .items
        .iter()
        .any(|item| item.contains("present and unreferenced")));
}

#[test]
fn post_settle_payload_activity_is_new_activity_in_buffer_and_segments_alike() {
    let dir = test_dir("payload-new-activity");
    let store = Store::open(
        &dir,
        Config {
            payload_threshold: Some(64),
            ..wal_config()
        },
    )
    .expect("opens");
    let secret = format!("reuploaded {}", "u".repeat(200));
    let reference = format!("sha256/{}", traza::payload::sha256_hex(secret.as_bytes()));
    store
        .ingest(span("t1", "a", json!({"prompt": secret})))
        .expect("ingests");
    store.flush().expect("seals");
    let status = store
        .erase(Subject::Payload {
            reference: reference.clone(),
        })
        .expect("erases");

    // Legitimate post-settle re-upload of the same content, under a NEW key.
    store
        .ingest(span("t2", "b", json!({"prompt": secret})))
        .expect("new data");
    let buffered = store.verify_erasure(status.erase.id).expect("receipt");
    assert_eq!(buffered.result, "erased");
    assert_eq!(domain(&buffered, "write-buffer").new_activity, 1);

    store.flush().expect("seals the new span");
    let sealed = store.verify_erasure(status.erase.id).expect("receipt");
    assert_eq!(
        sealed.result,
        "erased",
        "the verdict must not flip because a seal moved the same span from \
         buffer to segment:\n{}",
        sealed.render_text()
    );
    assert_eq!(domain(&sealed, "segments").new_activity, 1);
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
                tenant: String::new(),
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
            tenant: String::new(),
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
            tenant: String::new(),
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
            tenant: String::new(),
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
            tenant: String::new(),
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
            tenant: String::new(),
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
            tenant: String::new(),
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
                .map(|(_tenant, trace, id)| span(trace, id, json!({})))
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
            tenant: String::new(),
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
        Self::spawn_with_tokens(data_dir, None)
    }

    fn spawn_with_tokens(data_dir: &Path, tokens: Option<&str>) -> Self {
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
            // The point is what survives WITHOUT a segment flush.
            .arg("--flush-spans")
            .arg("1000000");
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

    /// SIGKILL: no unwinding, no flush on the way out. Idempotent with the
    /// `Drop` kill that follows.
    fn kill_hard(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn request(&self, method: &str, target: &str, body: Option<&Value>) -> (u16, Value) {
        self.request_as(None, method, target, body)
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

#[test]
fn http_never_reports_a_suppressed_span_as_accepted() {
    let dir = test_dir("http-suppressed");
    {
        let store = Store::open(&dir, wal_config()).expect("opens");
        store.ingest(span("txp", "s1", json!({}))).expect("s1");
        store.flush().expect("seals");
    }
    let line = json!({
        "op": "erase", "schema": 1, "id": 1, "requested_unix_ns": 123,
        "subject": {"kind": "trace", "trace_id": "txp"},
        "span_keys": [["txp", "s1"]], "payload_refs": [],
    });
    std::fs::write(dir.join("tombstones.jsonl"), format!("{line}\n")).expect("plants");

    // The maintenance tick settles the pending erasure at its first firing
    // (five seconds in); these requests land well inside the window.
    let server = Server::spawn(&dir);
    let (status, body) = server.request(
        "POST",
        "/v1/spans",
        Some(&json!([
            {"trace_id": "txp", "span_id": "s2", "name": "n", "service": "svc",
             "start_time_unix_nano": 1_000u64, "end_time_unix_nano": 2_000u64},
            {"trace_id": "kept", "span_id": "k1", "name": "n", "service": "svc",
             "start_time_unix_nano": 1_000u64, "end_time_unix_nano": 2_000u64},
        ])),
    );
    assert_eq!(status, 200);
    assert_eq!(
        body["accepted"],
        json!(1),
        "accepted counts what was stored, nothing else: {body}"
    );
    assert_eq!(body["suppressed"], json!(1), "{body}");

    // The OTLP JSON surface tells the same truth in its own vocabulary.
    let (status, body) = server.request(
        "POST",
        "/v1/traces",
        Some(&json!({"resourceSpans": [{
            "resource": {"attributes": [{"key": "service.name",
                "value": {"stringValue": "svc"}}]},
            "scopeSpans": [{"spans": [{
                "traceId": "747870747870747870747870747870ff",
                "spanId": "73330000000000ff",
                "name": "n", "startTimeUnixNano": "1000", "endTimeUnixNano": "2000"
            }]}]
        }]})),
    );
    assert_eq!(status, 200);
    // This OTLP span's trace id is not the covered one, so it flows —
    // the point here is the response SHAPE stays plain full success.
    assert_eq!(body["partialSuccess"], json!({}), "{body}");
}

#[test]
fn erasure_requires_the_admin_scope_not_merely_the_write_scope() {
    let dir = test_dir("admin-scope");
    let server = Server::spawn_with_tokens(
        &dir,
        Some("rw:writer-token,ro:reader-token,admin:root-token"),
    );

    let (status, _) = server.request_as(
        Some("writer-token"),
        "POST",
        "/v1/spans",
        Some(
            &json!([{ "trace_id": "t", "span_id": "s", "name": "n", "service": "svc",
            "start_time_unix_nano": 1_000u64, "end_time_unix_nano": 2_000u64 }]),
        ),
    );
    assert_eq!(status, 200, "the write scope still ingests");

    let erase_body = json!({"subject": {"kind": "trace", "trace_id": "t"}});
    // Every collector holds an rw token; a credential minted to write
    // telemetry must not be able to destroy it.
    let (status, _) = server.request_as(
        Some("writer-token"),
        "POST",
        "/v1/erasures",
        Some(&erase_body),
    );
    assert_eq!(status, 403, "rw must not erase");
    let (status, _) = server.request_as(
        Some("reader-token"),
        "POST",
        "/v1/erasures",
        Some(&erase_body),
    );
    assert_eq!(status, 403, "ro must not erase");
    let (status, _) = server.request_as(None, "POST", "/v1/erasures", Some(&erase_body));
    assert_eq!(status, 401, "no token is no token");

    let (status, settled) = server.request_as(
        Some("root-token"),
        "POST",
        "/v1/erasures",
        Some(&erase_body),
    );
    assert_eq!(status, 200, "admin erases: {settled}");
    assert_eq!(settled["settle"]["spans_removed"], json!(1));

    // Reading the tombstone log and the receipt is not destructive; the
    // read scope keeps it.
    let (status, _) = server.request_as(Some("reader-token"), "GET", "/v1/erasures", None);
    assert_eq!(status, 200);
    let (status, receipt) =
        server.request_as(Some("reader-token"), "GET", "/v1/erasures/1/verify", None);
    assert_eq!(status, 200);
    assert_eq!(receipt["result"], json!("erased"));
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
