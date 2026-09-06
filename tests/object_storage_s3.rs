//! End-to-end archive test against a REAL S3-compatible backend.
//!
//! Marked `#[ignore]` because it talks to a live service and needs a
//! disposable harness; run it explicitly:
//!
//! ```sh
//! export TRAZA_TEST_S3_ENDPOINT=http://127.0.0.1:9000   # MinIO etc.
//! export TRAZA_TEST_S3_BUCKET=traza-test
//! export AWS_ACCESS_KEY_ID=... AWS_SECRET_ACCESS_KEY=... AWS_REGION=us-east-1
//! cargo test --locked --features object-storage --test object_storage_s3 -- --ignored
//! ```
//!
//! Invoked without the environment it FAILS — an explicitly requested run
//! must never report a pass it did not earn. Plain HTTP is enabled only
//! when the endpoint says `http://` — the explicit local-testing opt-in the
//! library requires. A freshly created bucket may need a moment; the test
//! retries its first remote operation briefly.

#![cfg(feature = "object-storage")]

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use traza::object_storage::{Backend, Remote, RemoteOptions, S3Options};
use traza::{Config, Durability, SpanFilter, Store};

fn test_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "traza-objs3-it-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).expect("dir");
    dir
}

#[test]
#[ignore = "requires disposable S3 harness"]
fn publish_query_verify_restore_delete_against_real_s3() {
    let endpoint = std::env::var("TRAZA_TEST_S3_ENDPOINT").expect(
        "TRAZA_TEST_S3_ENDPOINT is not set; this test was invoked explicitly and needs \
         the disposable S3 harness",
    );
    let bucket = std::env::var("TRAZA_TEST_S3_BUCKET").expect(
        "TRAZA_TEST_S3_BUCKET is not set; this test was invoked explicitly and needs \
         the disposable S3 harness",
    );

    let dir = test_dir("s3");
    let store = Store::open(
        &dir,
        Config {
            durability: Durability::Buffered,
            compaction: None,
            ..Config::default()
        },
    )
    .expect("open");
    let spans: Vec<traza::Span> = (0..200u64)
        .map(|index| {
            serde_json::from_value(serde_json::json!({
                "trace_id": format!("t{index}"), "span_id": "s", "name": "s",
                "service": "svc", "start_time_ns": 1_000 + index,
                "end_time_ns": 2_000 + index,
                "attributes": {"marker": "s3"},
            }))
            .expect("span")
        })
        .collect();
    store.ingest_batch(spans).expect("ingest");
    store.flush().expect("flush");
    store.pin_for_object_archive("arch").expect("pin");

    let snapshot = format!(
        "it-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let make_remote = || {
        let mut options = RemoteOptions::new(Backend::S3(S3Options {
            bucket: bucket.clone(),
            region: None,
            allow_http: endpoint.starts_with("http://"),
            force_path_style: true,
            endpoint: Some(endpoint.clone()),
        }));
        options.prefix = "traza-it".to_owned();
        options.store_identity = "traza-s3-it".to_owned();
        Remote::open(options)
    };
    let remote = make_remote().expect("remote");

    let outcome = (|| -> Result<(), String> {
        // A just-created bucket can lag behind its creation call on some
        // harnesses. Retry only a read; a failed publication can claim an
        // immutable ID and must not be blindly retried under that name.
        for attempt in 0..5 {
            match remote.list_snapshots() {
                Ok(_) => break,
                Err(error) if attempt < 4 => {
                    eprintln!("readiness attempt {attempt} failed ({error}); retrying");
                    std::thread::sleep(Duration::from_secs(2));
                }
                Err(error) => return Err(format!("readiness: {error}")),
            }
        }
        let receipt = remote
            .publish_pin(&store.pin_path("arch"), &snapshot)
            .map_err(|error| format!("publish: {error}"))?;
        if receipt.files < 2 {
            return Err(format!("suspiciously few files: {}", receipt.files));
        }

        let listed = remote
            .list_snapshots()
            .map_err(|error| format!("list: {error}"))?;
        if !listed
            .iter()
            .any(|entry| entry.id == snapshot && entry.complete)
        {
            return Err("the published snapshot is not listed complete".to_owned());
        }

        let verify = remote
            .verify_snapshot(&snapshot, true)
            .map_err(|error| format!("verify: {error}"))?;
        if !verify.problems.is_empty() {
            return Err(format!("deep verify: {:?}", verify.problems));
        }

        // ReadStats counters are Remote-wide; exclude publication and deep
        // verification when checking reader transfer accounting.
        let reader = make_remote().map_err(|error| format!("reader: {error}"))?;
        let view = reader
            .open_snapshot(&snapshot)
            .map_err(|error| format!("open: {error}"))?;
        if view.manifest_sha256() != receipt.manifest_sha256 {
            return Err("the fetched manifest does not hash to the published digest".to_owned());
        }
        let all = view
            .query(&SpanFilter::default())
            .map_err(|error| format!("query: {error}"))?;
        if all.len() != 200 {
            return Err(format!("expected 200 spans, got {}", all.len()));
        }
        let one = view
            .get_trace(None, "t42")
            .map_err(|error| format!("trace: {error}"))?;
        if one.len() != 1 {
            return Err(format!("expected trace t42, got {} spans", one.len()));
        }
        let stats = view.read_stats();
        if stats.fetched_bytes == 0 || stats.fetched_bytes > stats.remote_total_bytes * 3 {
            return Err(format!("implausible transfer accounting: {stats:?}"));
        }

        let target = test_dir("s3-restore").join("staged");
        remote
            .restore_snapshot(&snapshot, &target)
            .map_err(|error| format!("restore: {error}"))?;
        let fresh = test_dir("s3-restore").join("store");
        let restored = Store::restore(&fresh, &target, Config::default())
            .map_err(|error| format!("install: {error}"))?;
        let count = restored
            .query(&SpanFilter::default())
            .map_err(|error| format!("restored query: {error}"))?
            .len();
        if count != 200 {
            return Err(format!("restored store holds {count} spans, expected 200"));
        }
        Ok(())
    })();

    // Best-effort remote cleanup happens whether or not the assertions
    // held, so a failing run does not leak objects into the shared test
    // bucket. Deletion permanently tombstones the (nonce-unique) id.
    let cleanup = remote
        .delete_snapshot(&snapshot)
        .or_else(|_| remote.cleanup_snapshot(&snapshot, true));
    outcome.expect("s3 round trip");
    cleanup.expect("cleanup");
}
