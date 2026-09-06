//! The object-storage archive, exercised through the public API against an
//! in-memory backend and a programmable fault backend.
//!
//! The oracle throughout is the LOCAL store: a published snapshot must
//! answer queries exactly as its source answered them at pin time —
//! last-write-wins across segments, tenant scoping, sessions, cursors,
//! content search, payload retrieval — and a restore must reproduce every
//! mutation domain (spans, annotations, payloads, eval records, settled
//! tombstones) through the restored store's own APIs. The failure tests
//! prove the publication contract (manifest last, objects verified, one
//! winner per name), the read contract (chunks verified on every read,
//! cache included, manifests bound to the generation they archived), the
//! deletion contract (permanent tombstones, ids never reused, resumable,
//! never a false success), and the runtime contract (safe inside a foreign
//! async runtime, budgets that never report success past expiry).

#![cfg(feature = "object-storage")]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use traza::object_storage::object_store::memory::InMemory;
use traza::object_storage::object_store::path::Path as ObjPath;
use traza::object_storage::object_store::{ObjectStore, ObjectStoreExt};
use traza::object_storage::testing::{block_on, Fault, FaultAction, FaultStore};
use traza::object_storage::{Backend, Error as ArchiveError, Remote, RemoteOptions};
use traza::{Config, Durability, SpanCursor, SpanFilter, Store};

fn test_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "traza-objstore-it-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("dir");
    dir
}

fn span_in(tenant: &str, trace: &str, id: &str, marker: &str, start: u64) -> traza::Span {
    let mut value = serde_json::json!({
        "trace_id": trace, "span_id": id, "name": "s", "service": "svc",
        "start_time_ns": start, "end_time_ns": start + 1_000,
        "attributes": {"marker": marker, "note": format!("word{marker} common")},
    });
    if !tenant.is_empty() {
        value["$tenant"] = serde_json::json!(tenant);
    }
    serde_json::from_value(value).expect("span")
}

fn config() -> Config {
    Config {
        durability: Durability::Buffered,
        // Segments must stay distinct so last-write-wins across segments is
        // actually exercised remotely.
        compaction: None,
        payload_threshold: Some(256),
        ..Config::default()
    }
}

fn remote_over(store: Arc<dyn ObjectStore>) -> Remote {
    Remote::open(RemoteOptions::new(Backend::Custom(store))).expect("remote")
}

/// Unsize helper: `as` cannot unsize an `Arc`, a typed return can.
fn dyn_store<S: ObjectStore>(store: Arc<S>) -> Arc<dyn ObjectStore> {
    store
}

/// The source store's whole fixture: every mutation domain populated.
struct SourceFixture {
    store: Store,
    /// The default tenant's offloaded prompt text.
    big: String,
    /// Tenant acme's offloaded prompt text — held by a dataset example
    /// after its source trace was erased.
    acme_big: String,
    /// The `sha256/<hex>` reference of `acme_big`.
    acme_reference: String,
    dataset: u64,
    version_id: String,
    experiment: u64,
    /// The settled erasure of acme's source trace.
    acme_erasure_id: u64,
}

/// Builds a source store exercising every mutation domain: two tenants
/// sharing a trace id, a key overwritten across two segments, session ids,
/// offloaded payloads, an annotation, a full eval fixture under tenant
/// `acme` (dataset → version with a payload-referencing example →
/// experiment → run → score), and TWO settled erasures — one plain trace
/// removal, one whose payload survives through the dataset's copy.
fn build_source(dir: &Path) -> SourceFixture {
    let store = Store::open(dir, config()).expect("open");
    let big = "P".repeat(2_000);
    let acme_big = "acme confidential prompt ".repeat(100);
    let mut with_payload = span_in("", "t-pay", "p1", "payload", 5_000);
    with_payload
        .attributes
        .insert("gen_ai.prompt".into(), serde_json::json!(big.clone()));
    let mut session_a = span_in("", "t1", "a", "v1", 1_000);
    session_a
        .attributes
        .insert("session.id".into(), serde_json::json!("sess-1"));
    let mut session_b = span_in("", "t1", "b", "keep", 1_100);
    session_b
        .attributes
        .insert("session.id".into(), serde_json::json!("sess-1"));
    let mut acme_source = span_in("acme", "acme-src", "s1", "acme-erased", 4_000);
    acme_source
        .attributes
        .insert("gen_ai.prompt".into(), serde_json::json!(acme_big.clone()));
    store
        .ingest_batch(vec![
            session_a,
            session_b,
            span_in("acme", "t1", "a", "acme-own", 1_200),
            with_payload,
            span_in("", "t-erase", "e1", "erased", 1_300),
            acme_source,
            span_in("acme", "acme-run", "r1", "acme-run", 4_100),
        ])
        .expect("ingest");
    store.flush().expect("flush");
    // Overwrite (tenant "", t1, a) in a LATER segment: the remote reader
    // must serve v2 and suppress v1 exactly as the local store does.
    let mut v2 = span_in("", "t1", "a", "v2", 1_000);
    v2.attributes
        .insert("session.id".into(), serde_json::json!("sess-1"));
    store.ingest_batch(vec![v2]).expect("ingest v2");
    store.flush().expect("flush 2");
    store
        .annotate(traza::annotations::Annotation {
            tenant: String::new(),
            session_id: String::new(),
            experiment_id: None,
            example_id: String::new(),
            trace_id: "t1".into(),
            span_id: "b".into(),
            name: "score".into(),
            value: serde_json::json!(1),
            source: "eval:test".into(),
            comment: String::new(),
            timestamp_ns: 100,
        })
        .expect("annotate");

    // ---- the eval domain, under tenant acme ------------------------------
    let acme_stored = store.get_trace("acme-src").expect("acme trace");
    let acme_ref_object = acme_stored[0].attributes["gen_ai.prompt"].clone();
    let acme_reference = acme_ref_object["$payload"]
        .as_str()
        .expect("offloaded")
        .to_owned();
    let dataset = store.create_dataset("acme", "curated").expect("dataset");
    let outcome = store
        .create_dataset_version(
            Some("acme"),
            dataset,
            None,
            None,
            vec![serde_json::from_value(serde_json::json!({
                "example_id": "ex-1",
                // The FULL reference object, so the example keeps the
                // offloaded bytes alive past the source trace's erasure.
                "input": {"prompt": acme_ref_object},
                "provenance": {"trace_id": "acme-src", "span_id": "s1"},
            }))
            .expect("example")],
        )
        .expect("version");
    let experiment = store
        .create_experiment(Some("acme"), dataset, &outcome.version_id, "exp", None)
        .expect("experiment");
    store
        .record_eval_run(Some("acme"), experiment, "ex-1", "acme-run", "r1")
        .expect("run");
    store
        .annotate(traza::annotations::Annotation {
            tenant: "acme".into(),
            session_id: String::new(),
            experiment_id: Some(experiment),
            example_id: "ex-1".into(),
            trace_id: "acme-run".into(),
            span_id: "r1".into(),
            name: "accuracy".into(),
            value: serde_json::json!(1.0),
            source: "eval:test".into(),
            comment: String::new(),
            timestamp_ns: 200,
        })
        .expect("score");

    // ---- two settled erasures -------------------------------------------
    let status = store
        .erase(traza::erasure::Subject::Trace {
            trace_id: "t-erase".into(),
            tenant: String::new(),
        })
        .expect("erase");
    assert!(status.settle.is_some(), "the erasure settled synchronously");
    // Erase acme's SOURCE trace; the run trace stays, and the dataset's
    // example keeps the payload bytes alive.
    let acme_status = store
        .erase(traza::erasure::Subject::Trace {
            trace_id: "acme-src".into(),
            tenant: "acme".into(),
        })
        .expect("acme erase");
    let settle = acme_status.settle.as_ref().expect("settled");
    assert_eq!(
        settle.payloads_retained.len(),
        1,
        "the dataset example keeps the blob alive: {:?}",
        settle.payloads_removed
    );

    SourceFixture {
        store,
        big,
        acme_big,
        acme_reference,
        dataset,
        version_id: outcome.version_id,
        experiment,
        acme_erasure_id: acme_status.erase.id,
    }
}

fn spans_key_sorted(mut spans: Vec<traza::Span>) -> Vec<(String, String, String, String)> {
    spans.sort_by(|left, right| {
        (&left.tenant, &left.trace_id, &left.span_id).cmp(&(
            &right.tenant,
            &right.trace_id,
            &right.span_id,
        ))
    });
    spans
        .into_iter()
        .map(|span| {
            let marker = span
                .attributes
                .get("marker")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_owned();
            (span.tenant, span.trace_id, span.span_id, marker)
        })
        .collect()
}

/// Pins the fixture's store for archiving and returns the pin path.
fn archive_pin(store: &Store) -> PathBuf {
    store.pin_for_object_archive("arch").expect("pin");
    store.pin_path("arch")
}

#[test]
fn a_published_snapshot_answers_exactly_like_its_source() {
    let dir = test_dir("oracle");
    let fixture = build_source(&dir);
    let store = &fixture.store;

    let expected_all = store.query(&SpanFilter::default()).expect("local all");
    let expected_acme = store
        .query(&SpanFilter {
            tenant: Some("acme".into()),
            ..SpanFilter::default()
        })
        .expect("local acme");
    let expected_trace = store.get_trace("t1").expect("local trace");
    let expected_content = store
        .query(&SpanFilter {
            content: Some("wordv2".into()),
            ..SpanFilter::default()
        })
        .expect("local content");
    let expected_session = store
        .query(&SpanFilter {
            session: Some("sess-1".into()),
            ..SpanFilter::default()
        })
        .expect("local session");
    // The overwritten key answers v2 locally; that exact answer must
    // survive the archive.
    assert!(expected_trace.iter().any(|span| span.span_id == "a"
        && span.tenant.is_empty()
        && span.attributes["marker"] == "v2"));
    assert!(!expected_all
        .iter()
        .any(|span| span.attributes.get("marker") == Some(&serde_json::json!("v1"))));
    // Both erased traces are gone locally, so they must be absent remotely.
    assert!(!expected_all.iter().any(|span| span.trace_id == "t-erase"));
    assert!(!expected_all.iter().any(|span| span.trace_id == "acme-src"));
    // The `$tenant` identity really is the serde identity, not an
    // attribute.
    assert!(expected_acme.iter().all(|span| span.tenant == "acme"));

    let pin_dir = archive_pin(store);
    let backing: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let remote = remote_over(backing);
    let receipt = remote.publish_pin(&pin_dir, "snap").expect("publish");
    assert!(receipt.objects >= 1);
    // Every pack byte uploaded was read back and re-hashed before the
    // manifest went up; the receipt's upload total additionally counts the
    // intent marker and the manifest themselves.
    assert!(receipt.verified_bytes > 0);
    assert!(receipt.uploaded_bytes > receipt.verified_bytes);
    let manifest = remote.inspect_snapshot("snap").expect("inspect");
    assert_eq!(
        manifest
            .objects
            .iter()
            .map(|object| object.bytes)
            .sum::<u64>(),
        receipt.verified_bytes,
        "the manifest accounts for exactly the verified bytes"
    );
    store.release_pin("arch").expect("release");

    let view = remote.open_snapshot("snap").expect("open snapshot");
    assert_eq!(view.segment_count(), 2, "both segments archived");
    assert_eq!(view.manifest_sha256(), receipt.manifest_sha256);

    // Whole-corpus equality.
    let remote_all = view.query(&SpanFilter::default()).expect("remote all");
    assert_eq!(spans_key_sorted(remote_all), spans_key_sorted(expected_all));
    // Tenant scoping — and the identity itself round-trips.
    let remote_acme = view
        .query(&SpanFilter {
            tenant: Some("acme".into()),
            ..SpanFilter::default()
        })
        .expect("remote acme");
    assert!(remote_acme.iter().all(|span| span.tenant == "acme"));
    assert_eq!(
        spans_key_sorted(remote_acme),
        spans_key_sorted(expected_acme)
    );
    // Content search.
    let remote_content = view
        .query(&SpanFilter {
            content: Some("wordv2".into()),
            ..SpanFilter::default()
        })
        .expect("remote content");
    assert_eq!(
        spans_key_sorted(remote_content),
        spans_key_sorted(expected_content)
    );
    // Session resolution.
    let remote_session = view
        .query(&SpanFilter {
            session: Some("sess-1".into()),
            ..SpanFilter::default()
        })
        .expect("remote session");
    assert_eq!(
        spans_key_sorted(remote_session),
        spans_key_sorted(expected_session)
    );
    // Trace lookup, operator view and tenant view.
    let remote_trace = view.get_trace(None, "t1").expect("remote trace");
    assert_eq!(
        spans_key_sorted(remote_trace),
        spans_key_sorted(expected_trace)
    );
    let remote_trace_acme = view
        .get_trace(Some("acme"), "t1")
        .expect("remote trace acme");
    assert_eq!(remote_trace_acme.len(), 1);
    assert_eq!(remote_trace_acme[0].attributes["marker"], "acme-own");
    assert_eq!(remote_trace_acme[0].tenant, "acme");

    // Cursor pagination walks the identical dataset in pages of one.
    let full = view.query(&SpanFilter::default()).expect("full");
    let mut paged: Vec<traza::Span> = Vec::new();
    let mut cursor: Option<SpanCursor> = None;
    loop {
        let page = view
            .query_after(
                &SpanFilter {
                    limit: Some(1),
                    ..SpanFilter::default()
                },
                cursor.as_ref(),
            )
            .expect("page");
        match page.into_iter().next() {
            Some(span) => {
                cursor = Some(SpanCursor::from(&span));
                paged.push(span);
            }
            None => break,
        }
    }
    assert_eq!(spans_key_sorted(paged), spans_key_sorted(full));

    // Payload retrieval, fully verified end to end — the default tenant's
    // span-held payload and acme's dataset-held one.
    let payload_span = view
        .query(&SpanFilter {
            attributes: vec![("marker".into(), serde_json::json!("payload"))],
            ..SpanFilter::default()
        })
        .expect("payload span");
    assert_eq!(payload_span.len(), 1);
    let reference = payload_span[0].attributes["gen_ai.prompt"]["$payload"]
        .as_str()
        .expect("offloaded reference")
        .to_owned();
    let bytes = view
        .payload(&reference)
        .expect("payload fetch")
        .expect("payload present");
    assert_eq!(bytes, fixture.big.as_bytes());
    let acme_bytes = view
        .payload(&fixture.acme_reference)
        .expect("acme payload fetch")
        .expect("retained for the dataset example");
    assert_eq!(acme_bytes, fixture.acme_big.as_bytes());
    assert!(view
        .payload("sha256/0000000000000000000000000000000000000000000000000000000000000000")
        .expect("absent payload")
        .is_none());
}

#[test]
fn publication_writes_the_manifest_last_and_verifies_objects_first() {
    let dir = test_dir("order");
    let fixture = build_source(&dir);
    let pin_dir = archive_pin(&fixture.store);
    let fault = Arc::new(FaultStore::new(Arc::new(InMemory::new())));
    let remote = remote_over(dyn_store(fault.clone()));
    remote.publish_pin(&pin_dir, "snap").expect("publish");

    let operations = fault.operations();
    let puts: Vec<&String> = operations
        .iter()
        .filter(|op| op.starts_with("put "))
        .collect();
    assert!(
        puts.last()
            .expect("puts happened")
            .contains("manifest.json"),
        "the manifest must be the LAST object written: {puts:?}"
    );
    // Every pack was read back (a get) after its put and before the
    // manifest put — the read-back verification.
    let manifest_at = operations
        .iter()
        .position(|op| op.starts_with("put ") && op.contains("manifest.json"))
        .expect("manifest put");
    for put in operations
        .iter()
        .enumerate()
        .filter(|(_, op)| op.starts_with("put ") && op.contains(".pack"))
    {
        let object = put.1.trim_start_matches("put ").to_owned();
        assert!(
            operations.iter().enumerate().any(|(at, op)| at > put.0
                && at < manifest_at
                && op.starts_with("get ")
                && op.contains(&object)),
            "{object} was never read back before the manifest: {operations:?}"
        );
    }
}

#[test]
fn a_failed_upload_leaves_no_visible_snapshot_and_cleanup_needs_the_ack() {
    let dir = test_dir("partial");
    let fixture = build_source(&dir);
    let pin_dir = archive_pin(&fixture.store);
    let fault = Arc::new(FaultStore::new(Arc::new(InMemory::new())));
    fault.add_fault(Fault {
        op: "put",
        substring: ".pack".into(),
        remaining: 1,
        action: FaultAction::Fail,
    });
    let remote = remote_over(dyn_store(fault.clone()));
    let error = remote
        .publish_pin(&pin_dir, "snap")
        .expect_err("the scripted fault must fail the publish");
    assert!(matches!(error, ArchiveError::Remote(_)), "{error}");

    // Invisible: no manifest, so the snapshot does not open and lists as
    // debris with its intent marker.
    assert!(matches!(
        remote.open_snapshot("snap"),
        Err(ArchiveError::NotFound(_))
    ));
    let listed = remote.list_snapshots().expect("list");
    let entry = listed
        .iter()
        .find(|entry| entry.id == "snap")
        .expect("listed");
    assert!(!entry.complete);
    assert!(
        entry.uploading,
        "the intent marker names the abandoned upload"
    );

    // Deleting an in-flight-looking snapshot is always refused…
    assert!(matches!(
        remote.delete_snapshot("snap"),
        Err(ArchiveError::Refused(_))
    ));
    // …cleanup without the quiescence acknowledgment is refused too…
    assert!(matches!(
        remote.cleanup_snapshot("snap", false),
        Err(ArchiveError::Refused(_))
    ));
    // …and with the acknowledgment the data goes and a permanent fence stays.
    remote.cleanup_snapshot("snap", true).expect("cleanup");
    let cleaned = remote.list_snapshots().expect("list after cleanup");
    let retired = cleaned
        .iter()
        .find(|entry| entry.id == "snap")
        .expect("tombstone");
    assert!(retired.deleted && !retired.complete && !retired.uploading);

    // A resumed old writer cannot share a new writer's object names.
    assert!(remote.publish_pin(&pin_dir, "snap").is_err());
    remote
        .publish_pin(&pin_dir, "snap-retry")
        .expect("retry with fresh id");
    let view = remote.open_snapshot("snap-retry").expect("open");
    assert!(view.query(&SpanFilter::default()).expect("query").len() >= 4);
}

#[test]
fn corrupted_and_truncated_objects_are_refused_on_read() {
    let dir = test_dir("corrupt");
    let fixture = build_source(&dir);
    let pin_dir = archive_pin(&fixture.store);
    let backing = Arc::new(InMemory::new());
    let remote = remote_over(dyn_store(backing.clone()));
    remote.publish_pin(&pin_dir, "snap").expect("publish");
    let object = ObjPath::from("snapshots/snap/objects/00000000.pack");
    let original = block_on(async {
        backing
            .get(&object)
            .await
            .expect("get")
            .bytes()
            .await
            .expect("bytes")
            .to_vec()
    });

    // Same length, one flipped byte: shallow verify passes on lengths, deep
    // verify and every read that touches the chunk refuse.
    let mut flipped = original.clone();
    let middle = flipped.len() / 2;
    flipped[middle] ^= 0xff;
    block_on(backing.put(&object, flipped.into())).expect("tamper");
    let deep = remote.verify_snapshot("snap", true).expect("deep verify");
    assert!(!deep.problems.is_empty(), "deep verify must catch the flip");
    let failure = remote.open_snapshot("snap").and_then(|view| {
        view.query(&SpanFilter::default())
            .map_err(ArchiveError::from)
    });
    assert!(failure.is_err(), "reads over tampered bytes must refuse");

    // Truncation: shorter object, shallow verify catches it and reads fail.
    let truncated = original[..original.len() / 2].to_vec();
    block_on(backing.put(&object, truncated.into())).expect("truncate");
    let shallow = remote
        .verify_snapshot("snap", false)
        .expect("shallow verify");
    assert!(
        !shallow.problems.is_empty(),
        "length mismatch must be reported"
    );
    let failure = remote.open_snapshot("snap").and_then(|view| {
        view.query(&SpanFilter::default())
            .map_err(ArchiveError::from)
    });
    assert!(failure.is_err(), "reads over truncated bytes must refuse");

    // Malformed manifest: unparseable bytes are refused, never guessed at.
    block_on(backing.put(
        &ObjPath::from("snapshots/snap/manifest.json"),
        b"not a manifest".to_vec().into(),
    ))
    .expect("clobber manifest");
    assert!(matches!(
        remote.open_snapshot("snap"),
        Err(ArchiveError::Corrupt(_))
    ));
}

#[test]
fn a_manifest_that_disagrees_with_its_archived_generation_is_refused() {
    let dir = test_dir("tamper-manifest");
    let fixture = build_source(&dir);
    let pin_dir = archive_pin(&fixture.store);
    let backing = Arc::new(InMemory::new());
    let remote = remote_over(dyn_store(backing.clone()));
    remote.publish_pin(&pin_dir, "snap").expect("publish");
    let manifest_path = ObjPath::from("snapshots/snap/manifest.json");
    let original: serde_json::Value = serde_json::from_slice(&block_on(async {
        backing
            .get(&manifest_path)
            .await
            .expect("get")
            .bytes()
            .await
            .expect("bytes")
            .to_vec()
    }))
    .expect("manifest json");

    // Substitution: rename one archived segment entry to a different,
    // syntactically canonical segment name. Structure and pack geometry
    // stay valid, so ONLY the binding to the archived generation manifest
    // can catch it — and it must, before any query is answered.
    let mut substituted = original.clone();
    let files = substituted["files"].as_array_mut().expect("files");
    let victim = files
        .iter_mut()
        .find(|file| {
            file["path"]
                .as_str()
                .is_some_and(|path| path.starts_with("segment-"))
        })
        .expect("a segment entry");
    victim["path"] = serde_json::json!("segment-00000000000000000099.seg");
    block_on(backing.put(
        &manifest_path,
        serde_json::to_vec(&substituted).expect("encode").into(),
    ))
    .expect("tamper");
    let error = remote.open_snapshot("snap").expect_err("must refuse");
    assert!(
        error.to_string().contains("generation manifest"),
        "the refusal names the binding: {error}"
    );
    // Verify reports it as a problem too, without needing a query.
    let verify = remote.verify_snapshot("snap", false).expect("verify runs");
    assert!(!verify.problems.is_empty(), "{verify:?}");

    // Omission: drop a segment entry entirely. Whatever refuses first —
    // pack-coverage geometry or the generation binding — the promise is
    // that a query-open over a partial store CANNOT succeed.
    let mut omitted = original.clone();
    let files = omitted["files"].as_array_mut().expect("files");
    let before = files.len();
    files.retain(|file| {
        !file["path"]
            .as_str()
            .is_some_and(|path| path.starts_with("segment-"))
    });
    assert!(files.len() < before, "something was omitted");
    block_on(backing.put(
        &manifest_path,
        serde_json::to_vec(&omitted).expect("encode").into(),
    ))
    .expect("tamper");
    assert!(
        remote.open_snapshot("snap").is_err(),
        "an omitted segment must fail the query-open, not shrink the answers"
    );
}

#[test]
fn one_name_has_one_publication_winner() {
    let dir = test_dir("winner");
    let fixture = build_source(&dir);
    let pin_dir = archive_pin(&fixture.store);
    let backing = Arc::new(InMemory::new());
    let remote = remote_over(dyn_store(backing.clone()));
    remote.publish_pin(&pin_dir, "snap").expect("first publish");
    // A second publisher of the same name loses, loudly.
    assert!(matches!(
        remote.publish_pin(&pin_dir, "snap"),
        Err(ArchiveError::AlreadyExists(_))
    ));
    // A publisher racing an IN-FLIGHT upload (intent marker, no manifest)
    // also loses at the marker's conditional create.
    block_on(backing.put(
        &ObjPath::from("snapshots/inflight/UPLOADING"),
        b"{}".to_vec().into(),
    ))
    .expect("plant marker");
    assert!(matches!(
        remote.publish_pin(&pin_dir, "inflight"),
        Err(ArchiveError::AlreadyExists(_))
    ));
    // Invalid snapshot ids never reach the remote.
    assert!(matches!(
        remote.publish_pin(&pin_dir, "Bad/Name"),
        Err(ArchiveError::Refused(_))
    ));
}

#[test]
fn deleted_snapshot_ids_are_tombstoned_and_never_reused() {
    let dir = test_dir("tombstone");
    let fixture = build_source(&dir);
    let pin_dir = archive_pin(&fixture.store);
    let backing = Arc::new(InMemory::new());
    // Cross-client: the publisher, the deleter, and the later would-be
    // republisher are separate Remote instances sharing only the bucket.
    let publisher = remote_over(dyn_store(backing.clone()));
    let deleter = remote_over(dyn_store(backing.clone()));
    let late_writer = remote_over(dyn_store(backing.clone()));

    publisher.publish_pin(&pin_dir, "snap").expect("publish");
    let receipt = deleter.delete_snapshot("snap").expect("delete");
    assert!(receipt.objects_deleted >= 2, "manifest and packs went");

    // The id is dead to every client, forever.
    assert!(matches!(
        publisher.open_snapshot("snap"),
        Err(ArchiveError::NotFound(_))
    ));
    let error = late_writer
        .publish_pin(&pin_dir, "snap")
        .expect_err("republish of a deleted id must refuse");
    assert!(error.to_string().contains("permanently retired"), "{error}");
    let listed = publisher.list_snapshots().expect("list");
    let entry = listed
        .iter()
        .find(|entry| entry.id == "snap")
        .expect("listed");
    assert!(entry.deleted, "the tombstone is visible in the listing");
    assert!(!entry.complete);

    // Idempotent: deleting again succeeds with nothing left to do.
    let again = deleter.delete_snapshot("snap").expect("idempotent");
    assert_eq!(again.objects_deleted, 0);
}

#[test]
fn an_interrupted_delete_resumes_and_stays_invisible_throughout() {
    let dir = test_dir("delete-resume");
    let fixture = build_source(&dir);
    let pin_dir = archive_pin(&fixture.store);
    let fault = Arc::new(FaultStore::new(Arc::new(InMemory::new())));
    let remote = remote_over(dyn_store(fault.clone()));
    remote.publish_pin(&pin_dir, "snap").expect("publish");

    // Interrupt the sweep: the tombstone and manifest removal already
    // landed (visibility first), and the reported failure is honest —
    // objects remain.
    fault.add_fault(Fault {
        op: "delete",
        substring: ".pack".into(),
        remaining: 1,
        action: FaultAction::Fail,
    });
    assert!(remote.delete_snapshot("snap").is_err());
    assert!(
        matches!(remote.open_snapshot("snap"), Err(ArchiveError::NotFound(_))),
        "a half-deleted snapshot is invisible, not half-readable"
    );

    // Resuming finishes it — from a DIFFERENT client, off the tombstone
    // alone — and the id stays permanently retired.
    let resumer = remote_over(dyn_store(fault.clone()));
    resumer.delete_snapshot("snap").expect("resume delete");
    let listed = resumer.list_snapshots().expect("list");
    let entry = listed
        .iter()
        .find(|entry| entry.id == "snap")
        .expect("listed");
    assert!(entry.deleted && !entry.complete && !entry.uploading);
    assert!(matches!(
        resumer.publish_pin(&pin_dir, "snap"),
        Err(ArchiveError::Refused(_))
    ));
}

#[test]
fn a_swept_publisher_aborts_instead_of_publishing_over_the_cleanup() {
    let dir = test_dir("swept-publisher");
    let fixture = build_source(&dir);
    let pin_dir = archive_pin(&fixture.store);
    let fault = Arc::new(FaultStore::new(Arc::new(InMemory::new())));
    // The publisher's pre-commit marker re-check is its ONLY head of the
    // marker; stall it long enough for a cleanup to sweep the upload.
    fault.add_fault(Fault {
        op: "head",
        substring: "UPLOADING".into(),
        remaining: 1,
        action: FaultAction::Delay(Duration::from_millis(1_500)),
    });
    let publisher = remote_over(dyn_store(fault.clone()));
    let janitor = remote_over(dyn_store(fault.clone()));

    let publish_thread = std::thread::spawn(move || publisher.publish_pin(&pin_dir, "snap"));
    // Let the publisher claim its marker and upload, then sweep it under
    // the explicit quiescence acknowledgment (deliberately violated here —
    // this test IS the race the acknowledgment exists to prevent, proving
    // the publisher's own guard catches the sequential case).
    std::thread::sleep(Duration::from_millis(400));
    janitor.cleanup_snapshot("snap", true).expect("cleanup");
    let outcome = publish_thread.join().expect("publisher thread");
    let error = outcome.expect_err("the swept publisher must abort");
    assert!(
        error.to_string().contains("cleanup swept")
            || error.to_string().contains("disappeared")
            || error.to_string().contains("permanently retired"),
        "{error}"
    );
    // Nothing became visible.
    assert!(matches!(
        janitor.open_snapshot("snap"),
        Err(ArchiveError::NotFound(_))
    ));
}

#[test]
fn foreign_identities_are_refused_and_expected_digests_are_enforced() {
    let dir = test_dir("identity");
    let fixture = build_source(&dir);
    let pin_dir = archive_pin(&fixture.store);
    let backing = Arc::new(InMemory::new());
    let mut options = RemoteOptions::new(Backend::Custom(dyn_store(backing.clone())));
    options.store_identity = "prod".into();
    let prod = Remote::open(options).expect("remote");
    let receipt = prod.publish_pin(&pin_dir, "snap").expect("publish");

    // A remote configured as a DIFFERENT store cannot read, verify, or
    // delete it.
    let mut options = RemoteOptions::new(Backend::Custom(dyn_store(backing.clone())));
    options.store_identity = "other".into();
    let other = Remote::open(options).expect("remote");
    assert!(matches!(
        other.open_snapshot("snap"),
        Err(ArchiveError::Refused(_))
    ));
    assert!(matches!(
        other.inspect_snapshot("snap"),
        Err(ArchiveError::Refused(_))
    ));
    assert!(matches!(
        other.verify_snapshot("snap", false),
        Err(ArchiveError::Refused(_))
    ));
    assert!(matches!(
        other.delete_snapshot("snap"),
        Err(ArchiveError::Refused(_))
    ));
    let listed = other.list_snapshots().expect("list");
    let entry = listed
        .iter()
        .find(|entry| entry.id == "snap")
        .expect("listed");
    assert!(entry.foreign, "listed, named, contents withheld");
    // An unconfigured identity accepts any.
    let anyone = remote_over(dyn_store(backing.clone()));
    anyone.open_snapshot("snap").expect("open");

    // The externally retained manifest digest: matching opens, anything
    // else refuses — the defense against a rewritten manifest.
    let mut options = RemoteOptions::new(Backend::Custom(dyn_store(backing.clone())));
    options.store_identity = "prod".into();
    options.expected_manifest_sha256 = Some(receipt.manifest_sha256.clone());
    Remote::open(options)
        .expect("remote")
        .open_snapshot("snap")
        .expect("the retained digest matches");
    let mut options = RemoteOptions::new(Backend::Custom(dyn_store(backing.clone())));
    options.expected_manifest_sha256 = Some("0".repeat(64));
    assert!(matches!(
        Remote::open(options).expect("remote").open_snapshot("snap"),
        Err(ArchiveError::Corrupt(_))
    ));

    // After the identity-holder deletes, the tombstone retains the
    // identity evidence: the foreign remote is still refused, not told
    // "not found".
    prod.delete_snapshot("snap").expect("delete");
    assert!(matches!(
        other.delete_snapshot("snap"),
        Err(ArchiveError::Refused(_))
    ));
}

#[test]
fn the_chunk_cache_is_bounded_hit_counted_and_tamper_evident() {
    let dir = test_dir("cache");
    let fixture = build_source(&dir);
    let pin_dir = archive_pin(&fixture.store);
    let backing: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let mut options = RemoteOptions::new(Backend::Custom(backing));
    options.cache_bytes = 2 << 20;
    let remote = Remote::open(options).expect("remote");
    remote.publish_pin(&pin_dir, "snap").expect("publish");

    let view = remote.open_snapshot("snap").expect("open");
    let _ = view.get_trace(None, "t1").expect("first read");
    let after_first = view.read_stats();
    assert!(after_first.cache_resident_bytes <= 2 << 20, "budget holds");
    let _ = view.get_trace(None, "t1").expect("second read");
    let after_second = view.read_stats();
    // A decoded-block hit can bypass the raw chunk cache entirely.
    assert_eq!(
        after_second.chunk_fetches, after_first.chunk_fetches,
        "the repeat fetched nothing new"
    );
    let first_payload = view
        .payload(&fixture.acme_reference)
        .expect("payload")
        .expect("present");
    let before_payload_repeat = view.read_stats();
    let second_payload = view
        .payload(&fixture.acme_reference)
        .expect("repeat payload")
        .expect("present");
    let after_payload_repeat = view.read_stats();
    assert_eq!(first_payload, second_payload);
    assert!(after_payload_repeat.cache_hits > before_payload_repeat.cache_hits);
    assert_eq!(
        after_payload_repeat.chunk_fetches,
        before_payload_repeat.chunk_fetches
    );
    // The residency truths the cache does NOT bound are reported, not
    // hidden.
    assert!(after_second.resident_index_bytes > 0);

    // A tampered cached chunk is refused on its next read: the cache is a
    // cost optimization, never a trust boundary.
    assert!(view.tamper_cached_chunk_for_tests(), "something was cached");
    assert!(
        view.payload(&fixture.acme_reference).is_err(),
        "the tampered cached chunk must fail its digest"
    );
}

#[test]
fn a_fresh_reader_needs_only_the_remote() {
    let dir = test_dir("fresh");
    let fixture = build_source(&dir);
    let pin_dir = archive_pin(&fixture.store);
    let backing = Arc::new(InMemory::new());
    let expected = {
        let remote = remote_over(dyn_store(backing.clone()));
        remote.publish_pin(&pin_dir, "snap").expect("publish");
        let view = remote.open_snapshot("snap").expect("open");
        spans_key_sorted(view.query(&SpanFilter::default()).expect("query"))
    };
    // The publisher's Remote (runtime, cache, counters) is gone; the local
    // store could be too. A brand-new reader reconstructs everything from
    // remote state alone.
    drop(fixture);
    let remote = remote_over(dyn_store(backing));
    let view = remote.open_snapshot("snap").expect("fresh open");
    assert_eq!(
        spans_key_sorted(view.query(&SpanFilter::default()).expect("query")),
        expected
    );
}

#[test]
fn the_adapter_is_safe_inside_a_foreign_async_runtime() {
    let dir = test_dir("embedded");
    let fixture = build_source(&dir);
    let pin_dir = archive_pin(&fixture.store);
    let backing = Arc::new(InMemory::new());
    remote_over(dyn_store(backing.clone()))
        .publish_pin(&pin_dir, "snap")
        .expect("publish");
    // Construct, read, and DROP the whole archive stack from inside a
    // foreign tokio runtime's async context. The calls block the outer
    // worker thread (that is what a synchronous call means); nothing may
    // panic — constructing a runtime inside a runtime and dropping one
    // inside an async context are the two classic tokio panics this
    // adapter's dedicated-thread design exists to avoid.
    block_on(async move {
        let remote = remote_over(dyn_store(backing));
        let view = remote.open_snapshot("snap").expect("open inside async");
        let spans = view
            .query(&SpanFilter::default())
            .expect("query inside async");
        assert!(!spans.is_empty());
        drop(view);
        drop(remote);
    });
}

#[test]
fn remote_queries_never_report_success_past_their_budget() {
    let dir = test_dir("deadline");
    let fixture = build_source(&dir);
    let pin_dir = archive_pin(&fixture.store);
    let remote = remote_over(dyn_store(Arc::new(InMemory::new())));
    remote.publish_pin(&pin_dir, "snap").expect("publish");
    let view = remote.open_snapshot("snap").expect("open");

    // A zero budget is already expired at the first check: every bounded
    // path must refuse — the plain scan, the session union (which used to
    // bypass the budget), and the trace walk — and none may return rows.
    let exhausted = Some(Duration::ZERO);
    for filter in [
        SpanFilter::default(),
        SpanFilter {
            session: Some("sess-1".into()),
            ..SpanFilter::default()
        },
    ] {
        match view.query_bounded(&filter, None, exhausted) {
            Err(traza::Error::DeadlineExceeded(_)) => {}
            other => panic!("expected DeadlineExceeded, got {other:?}"),
        }
    }
    match view.get_trace_bounded(None, "t1", exhausted) {
        Err(traza::Error::DeadlineExceeded(_)) => {}
        other => panic!("expected DeadlineExceeded, got {other:?}"),
    }

    // And with a real budget the same queries answer.
    let generous = Some(Duration::from_secs(60));
    assert!(!view
        .query_bounded(&SpanFilter::default(), None, generous)
        .expect("bounded query")
        .is_empty());
    assert!(!view
        .get_trace_bounded(None, "t1", generous)
        .expect("bounded trace")
        .is_empty());
}

#[test]
fn restore_reproduces_every_domain_and_refuses_existing_targets() {
    let source_dir = test_dir("restore-src");
    let restored_parent = test_dir("restore-dst");
    let fixture = build_source(&source_dir);
    let store = &fixture.store;
    let annotations_before = store.annotations("t1", None, None).expect("annotations");
    assert_eq!(annotations_before.len(), 1);
    let pin_dir = archive_pin(store);
    let backing = Arc::new(InMemory::new());
    let remote = remote_over(dyn_store(backing));
    remote.publish_pin(&pin_dir, "snap").expect("publish");

    let target = restored_parent.join("staged");
    let receipt = remote.restore_snapshot("snap", &target).expect("restore");
    assert!(receipt.files >= 4);

    // Byte-exactness against the pin it came from, file by file.
    let pin_manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(pin_dir.join("state-manifest.json")).expect("pin manifest"),
    )
    .expect("manifest json");
    for file in pin_manifest["files"].as_array().expect("files") {
        let path = file["path"].as_str().expect("path");
        let relative = path.replace('/', std::path::MAIN_SEPARATOR_STR);
        assert_eq!(
            fs::read(pin_dir.join(&relative)).expect("pin file"),
            fs::read(target.join(&relative)).expect("restored file"),
            "{path} must restore byte-identically"
        );
    }

    // Restoring over anything that exists is refused.
    assert!(matches!(
        remote.restore_snapshot("snap", &target),
        Err(ArchiveError::Refused(_))
    ));

    // The restored directory installs and serves EVERY domain through the
    // restored store's own APIs — under the EXACT ids the source assigned,
    // because id persistence is what keeps receipts and references valid.
    store.release_pin("arch").expect("release");
    let expected = spans_key_sorted(store.query(&SpanFilter::default()).expect("local"));
    let dataset_id = fixture.dataset;
    let version_id = fixture.version_id.clone();
    let experiment_id = fixture.experiment;
    let erasure_id = fixture.acme_erasure_id;
    let acme_reference = fixture.acme_reference.clone();
    let acme_big = fixture.acme_big.clone();
    drop(fixture);
    let fresh_root = restored_parent.join("fresh-store");
    let restored = Store::restore(&fresh_root, &target, config()).expect("install");

    // Spans, including the cross-segment overwrite and tenant identities.
    assert_eq!(
        spans_key_sorted(restored.query(&SpanFilter::default()).expect("query")),
        expected
    );
    let acme_spans = restored
        .query(&SpanFilter {
            tenant: Some("acme".into()),
            ..SpanFilter::default()
        })
        .expect("acme");
    assert!(!acme_spans.is_empty() && acme_spans.iter().all(|span| span.tenant == "acme"));

    // Annotations.
    let annotations = restored.annotations("t1", None, None).expect("annotations");
    assert_eq!(annotations.len(), 1, "annotations restored");

    // The tombstone log: both settled erasures, and the acme one under its
    // original id.
    let erasures = restored.erasures().expect("erasures");
    assert_eq!(erasures.len(), 2, "both settled tombstones restored");
    let acme_status = restored
        .erasure_status(erasure_id)
        .expect("status")
        .expect("recorded under its original id");
    assert!(acme_status.settle.is_some());
    assert!(acme_status.erase.subject.describe().contains("acme-src"));

    // The eval domain, tenant-scoped, under the original ids: dataset,
    // version (with its example), experiment, run, score.
    let dataset = restored
        .dataset(Some("acme"), dataset_id)
        .expect("dataset fetch")
        .expect("dataset present under its original id");
    assert_eq!(dataset.versions.len(), 1);
    assert_eq!(dataset.versions[0].version_id, version_id);
    let version = restored
        .dataset_version(Some("acme"), dataset_id, &version_id)
        .expect("version fetch")
        .expect("version present")
        .expect("not tombstoned");
    assert_eq!(version.bodies.len(), 1, "the example restored");
    assert!(restored
        .experiment(Some("acme"), experiment_id)
        .expect("experiment fetch")
        .is_some());
    let runs = restored
        .eval_runs(Some("acme"), experiment_id)
        .expect("runs fetch")
        .expect("experiment exists");
    assert_eq!(runs.len(), 1, "the recorded run restored");
    assert_eq!(runs[0].trace_id, "acme-run");
    let scores = restored
        .experiment_scores(Some("acme"), experiment_id, None)
        .expect("scores fetch")
        .expect("experiment exists");
    assert_eq!(scores.len(), 1, "the run-addressed score restored");
    assert_eq!(scores[0].name, "accuracy");

    // The example still carries the FULL reference object, and the
    // dataset-held payload restores to its exact bytes through the
    // tenant-scoped API — invisible to a foreign tenant.
    let restored_reference = version.bodies[0].body.input["prompt"]["$payload"]
        .as_str()
        .expect("reference in the example");
    assert_eq!(restored_reference, acme_reference);
    let payload = restored
        .payload_in(Some("acme"), &acme_reference)
        .expect("payload fetch")
        .expect("retained bytes restored");
    assert_eq!(payload, acme_big.as_bytes());
    assert!(restored
        .payload_in(Some("evil"), &acme_reference)
        .expect("foreign fetch")
        .is_none());
    assert!(restored
        .dataset(Some("evil"), dataset_id)
        .expect("foreign dataset fetch")
        .is_none());
}

#[test]
fn a_target_appearing_mid_restore_is_refused_not_replaced() {
    let dir = test_dir("restore-race");
    let parent = test_dir("restore-race-dst");
    let fixture = build_source(&dir);
    let pin_dir = archive_pin(&fixture.store);
    let fault = Arc::new(FaultStore::new(Arc::new(InMemory::new())));
    {
        let remote = remote_over(dyn_store(fault.clone()));
        remote.publish_pin(&pin_dir, "snap").expect("publish");
    }
    // Stall one data fetch long enough to create the target mid-restore.
    fault.add_fault(Fault {
        op: "get",
        substring: ".pack".into(),
        remaining: 1,
        action: FaultAction::Delay(Duration::from_millis(1_200)),
    });
    let remote = remote_over(dyn_store(fault));
    let target = parent.join("appears");
    let restore_target = target.clone();
    let thread = std::thread::spawn(move || remote.restore_snapshot("snap", &restore_target));
    std::thread::sleep(Duration::from_millis(300));
    fs::write(&target, b"i got here first").expect("occupy target");
    let error = thread
        .join()
        .expect("restore thread")
        .expect_err("must refuse the occupied target");
    assert!(matches!(error, ArchiveError::Refused(_)), "{error}");
    assert_eq!(
        fs::read(&target).expect("target intact"),
        b"i got here first",
        "the occupying file was not replaced"
    );
    // No staging debris left beside it.
    let leftovers: Vec<_> = fs::read_dir(&parent)
        .expect("read dir")
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with('.'))
        .collect();
    assert!(leftovers.is_empty(), "staging removed: {leftovers:?}");
}

#[test]
fn remote_operations_are_time_bounded() {
    let dir = test_dir("timeout");
    let fixture = build_source(&dir);
    let pin_dir = archive_pin(&fixture.store);
    let fault = Arc::new(FaultStore::new(Arc::new(InMemory::new())));
    {
        let remote = remote_over(dyn_store(fault.clone()));
        remote.publish_pin(&pin_dir, "snap").expect("publish");
    }
    fault.add_fault(Fault {
        op: "get",
        substring: "manifest.json".into(),
        remaining: 1,
        action: FaultAction::Delay(Duration::from_secs(5)),
    });
    let mut options = RemoteOptions::new(Backend::Custom(dyn_store(fault)));
    options.op_timeout = Duration::from_millis(200);
    let remote = Remote::open(options).expect("remote");
    let started = std::time::Instant::now();
    let error = remote.open_snapshot("snap").expect_err("must time out");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "bounded, not the fault's 5 s"
    );
    assert!(error.to_string().contains("timed out"), "{error}");
}

#[test]
fn narrow_queries_fetch_a_fraction_of_the_archive() {
    let dir = test_dir("savings");
    // Offloading disabled so the bulk text stays IN the records region: the
    // measurement is about ranged reads inside large segments, not about
    // payload files a trace lookup never touches.
    let store = Store::open(
        &dir,
        Config {
            payload_threshold: None,
            ..config()
        },
    )
    .expect("open");
    let mut state = 0x9e3779b97f4a7c15_u64;
    let mut noise = |len: usize| -> String {
        let mut out = String::with_capacity(len);
        while out.len() < len {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            out.push_str(&format!("{state:016x}"));
        }
        out.truncate(len);
        out
    };
    // One large segment on purpose: chunk-granular reads only save I/O when
    // the addressed regions (header, indexes, one record block) are a small
    // slice of the file, which is the workload shape archives exist for.
    let spans: Vec<traza::Span> = (0..3_000u64)
        .map(|index| {
            let mut span = span_in(
                "",
                &format!("trace-{index}"),
                "s",
                "bulk",
                1_000_000 + index,
            );
            span.attributes
                .insert("blob".into(), serde_json::json!(noise(4_096)));
            span
        })
        .collect();
    store.ingest_batch(spans).expect("ingest");
    store.flush().expect("flush");
    let pin_dir = archive_pin(&store);
    let backing: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let remote = remote_over(Arc::clone(&backing));
    remote.publish_pin(&pin_dir, "snap").expect("publish");

    // Transfer counters are Remote-wide: isolate reader economics from the
    // publication's mandatory full read-back verification.
    let reader = remote_over(backing);
    let view = reader.open_snapshot("snap").expect("open");
    let stats_open = view.read_stats();
    let hit = view.get_trace(None, "trace-1500").expect("trace");
    assert_eq!(hit.len(), 1, "the needle is found");
    let stats = view.read_stats();
    assert!(
        stats.remote_total_bytes > 8 << 20,
        "the corpus is big enough to make the measurement meaningful: {} bytes",
        stats.remote_total_bytes
    );
    assert!(
        stats.fetched_bytes < stats.remote_total_bytes * 3 / 4,
        "a point lookup must not approach a full download: fetched {} of {} \
         (open alone fetched {})",
        stats.fetched_bytes,
        stats.remote_total_bytes,
        stats_open.fetched_bytes,
    );
}
