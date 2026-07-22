//! End-to-end tests for the server CLI backed by the real storage engine.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before the Unix epoch")
            .as_nanos();
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "traza-server-on-engine-{label}-{}-{nonce}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("failed to create test database directory");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn run_server(database: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_traza-server"))
        .arg("--db")
        .arg(database)
        .args(arguments)
        .output()
        .expect("failed to launch the traza-server binary")
}

fn output_details(output: &Output) -> String {
    format!(
        "status: {}; stdout: {:?}; stderr: {:?}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn write_trace(database: &Path, trace_id: &str, payload: &str) -> Output {
    run_server(database, &["write", trace_id, payload])
}

fn read_trace(database: &Path, trace_id: &str) -> Output {
    run_server(database, &["read", trace_id])
}

#[test]
fn server_write_round_trip_uses_engine() {
    let database = TestDirectory::new("write-round-trip");
    let trace_id = "trace-write-round-trip-1847";
    let payload = r#"{"service":"checkout","duration_ms":17}"#;

    let write = write_trace(database.path(), trace_id, payload);
    assert!(
        write.status.success(),
        "server write failed: {}",
        output_details(&write)
    );

    let read = read_trace(database.path(), trace_id);
    assert!(
        read.status.success(),
        "engine-backed read failed: {}",
        output_details(&read)
    );
    assert_eq!(
        String::from_utf8_lossy(&read.stdout).trim(),
        payload,
        "engine read returned a different payload"
    );
}

#[test]
fn server_read_query_round_trip_uses_engine() {
    let database = TestDirectory::new("read-query");
    let wanted_id = "trace-inventory-2901";
    let other_id = "trace-inventory-2902";
    let wanted_payload = r#"{"operation":"inventory.lookup","result":"available"}"#;
    let other_payload = r#"{"operation":"inventory.lookup","result":"unavailable"}"#;

    for (trace_id, payload) in [(wanted_id, wanted_payload), (other_id, other_payload)] {
        let write = write_trace(database.path(), trace_id, payload);
        assert!(
            write.status.success(),
            "fixture write failed: {}",
            output_details(&write)
        );
    }

    let read = read_trace(database.path(), wanted_id);
    assert!(
        read.status.success(),
        "server query failed: {}",
        output_details(&read)
    );
    assert_eq!(
        String::from_utf8_lossy(&read.stdout).trim(),
        wanted_payload,
        "read returned the payload belonging to the wrong trace"
    );
}

#[test]
fn server_reopen_preserves_spans() {
    let database = TestDirectory::new("reopen-persistence");
    let trace_id = "trace-persisted-731";
    let payload = r#"{"span":"persisted-span","sequence":731}"#;

    let write = write_trace(database.path(), trace_id, payload);
    assert!(
        write.status.success(),
        "initial server process failed to persist the trace: {}",
        output_details(&write)
    );

    // Each invocation is a separate process, so this read reopens the engine.
    let reopened_read = read_trace(database.path(), trace_id);
    assert!(
        reopened_read.status.success(),
        "fresh server process failed to reopen the database: {}",
        output_details(&reopened_read)
    );
    assert_eq!(
        String::from_utf8_lossy(&reopened_read.stdout).trim(),
        payload,
        "fresh server process returned different persisted data"
    );
}

#[test]
fn server_invalid_write_preserves_error_contract() {
    let database = TestDirectory::new("invalid-write");
    let output = write_trace(database.path(), "", r#"{"span":"invalid"}"#);

    assert!(
        !output.status.success(),
        "an empty trace id was unexpectedly accepted: {}",
        output_details(&output)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_ascii_lowercase().contains("trace id")
            && stderr.to_ascii_lowercase().contains("required"),
        "invalid-write error must identify the empty trace id: {}",
        output_details(&output)
    );
}

#[test]
fn server_missing_trace_preserves_error_contract() {
    let database = TestDirectory::new("missing-trace");
    let missing_id = "trace-that-does-not-exist-404";
    let output = read_trace(database.path(), missing_id);

    assert!(
        !output.status.success(),
        "a missing trace was unexpectedly reported as present: {}",
        output_details(&output)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(missing_id)
            && (stderr.to_ascii_lowercase().contains("not found")
                || stderr.to_ascii_lowercase().contains("missing")),
        "missing-trace error must identify the requested trace: {}",
        output_details(&output)
    );
}
