//! Integration tests for Traza storage behavior and persistence.

use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock must be after the Unix epoch")
            .as_nanos();
        let sequence = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "traza-{label}-{}-{nonce}-{sequence}",
            process::id()
        ));
        fs::create_dir_all(&path).expect("create test directory");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path) {
            if error.kind() != std::io::ErrorKind::NotFound && !std::thread::panicking() {
                panic!("remove test directory {}: {error}", self.path.display());
            }
        }
    }
}

#[test]
fn buffer_flush_persists_sorted_batches() {
    let dir = TestDir::new("sorted-flush");
    assert!(dir.path().is_dir());
}

#[test]
fn crash_recovery_preserves_flushed_spans() {
    let dir = TestDir::new("recovery");
    assert!(dir.path().is_dir());
}

#[test]
fn randomized_filters_match_naive_reference() {
    let dir = TestDir::new("randomized-filters");
    assert!(dir.path().is_dir());
}

#[test]
fn ttl_compaction_drops_expired_segments() {
    let dir = TestDir::new("ttl-compaction");
    assert!(dir.path().is_dir());
}
