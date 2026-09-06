//! Multipart publication must abort its available upload handle after failure.

#![cfg(feature = "object-storage")]

use std::fmt;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::stream::BoxStream;
use traza::object_storage::object_store::memory::InMemory;
use traza::object_storage::object_store::path::Path;
use traza::object_storage::object_store::{
    self, CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult, UploadPart,
};
use traza::object_storage::testing::block_on;
use traza::object_storage::{Backend, Remote, RemoteOptions, PACK_TARGET_BYTES};
use traza::{Config, Durability, Store};

#[derive(Clone, Copy, Debug)]
enum Failure {
    SecondPartError,
    SecondPartTimeout,
    CompletionError,
}

#[derive(Debug, Default)]
struct Counts {
    created: AtomicUsize,
    part_calls: AtomicUsize,
    parts_completed: AtomicUsize,
    bytes_submitted: AtomicUsize,
    complete_calls: AtomicUsize,
    abort_calls: AtomicUsize,
    abort_completed: AtomicUsize,
    location: Mutex<Option<Path>>,
}

fn injected_error() -> object_store::Error {
    object_store::Error::Generic {
        store: "multipart-test",
        source: "injected multipart failure".into(),
    }
}

#[derive(Debug)]
struct FaultUpload {
    inner: Box<dyn MultipartUpload>,
    counts: Arc<Counts>,
    failure: Failure,
}

#[async_trait::async_trait]
impl MultipartUpload for FaultUpload {
    fn put_part(&mut self, data: PutPayload) -> UploadPart {
        let index = self.counts.part_calls.fetch_add(1, Ordering::SeqCst);
        self.counts
            .bytes_submitted
            .fetch_add(data.content_length(), Ordering::SeqCst);
        // The first part reaches the backing upload. Fail the second to
        // exercise cleanup with an actual successfully uploaded part present.
        if index == 1 {
            match self.failure {
                Failure::SecondPartError => return Box::pin(async { Err(injected_error()) }),
                Failure::SecondPartTimeout => {
                    return Box::pin(async {
                        tokio::time::sleep(Duration::from_secs(10)).await;
                        Err(injected_error())
                    });
                }
                Failure::CompletionError => {}
            }
        }
        let part = self.inner.put_part(data);
        let counts = Arc::clone(&self.counts);
        Box::pin(async move {
            part.await?;
            counts.parts_completed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    async fn complete(&mut self) -> object_store::Result<PutResult> {
        self.counts.complete_calls.fetch_add(1, Ordering::SeqCst);
        match self.failure {
            Failure::CompletionError => Err(injected_error()),
            _ => self.inner.complete().await,
        }
    }

    async fn abort(&mut self) -> object_store::Result<()> {
        self.counts.abort_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.abort().await?;
        self.counts.abort_completed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[derive(Debug)]
struct MultipartFaultStore {
    inner: Arc<InMemory>,
    counts: Arc<Counts>,
    failure: Failure,
}

impl fmt::Display for MultipartFaultStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MultipartFaultStore({:?})", self.failure)
    }
}

#[async_trait::async_trait]
impl ObjectStore for MultipartFaultStore {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.inner.put_opts(location, payload, options).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        options: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        let inner = self.inner.put_multipart_opts(location, options).await?;
        self.counts.created.fetch_add(1, Ordering::SeqCst);
        *self.counts.location.lock().expect("multipart path") = Some(location.clone());
        Ok(Box::new(FaultUpload {
            inner,
            counts: Arc::clone(&self.counts),
            failure: self.failure,
        }))
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

struct Fixture {
    root: PathBuf,
    pin: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "traza-multipart-test-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&root).expect("fixture directory");
        let mut fixture = Self {
            root,
            pin: PathBuf::new(),
        };
        let store = Store::open(
            &fixture.root,
            Config {
                durability: Durability::Buffered,
                compaction: None,
                payload_threshold: None,
                ..Config::default()
            },
        )
        .expect("source store");
        // Same deterministic, poorly compressible inline corpus as the large
        // ranged-read test: no external fixture or payload offloading.
        let mut state = 0x9e3779b97f4a7c15_u64;
        let spans = (0..3_000u64)
            .map(|index| {
                use std::fmt::Write;
                let mut noise = String::with_capacity(4_096);
                for _ in 0..256 {
                    state = state
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    write!(&mut noise, "{state:016x}").expect("noise");
                }
                serde_json::from_value(serde_json::json!({
                    "trace_id": format!("trace-{index}"), "span_id": "s",
                    "name": "s", "service": "svc", "start_time_ns": 1_000_000 + index,
                    "end_time_ns": 1_001_000 + index, "attributes": {"blob": noise},
                }))
                .expect("synthetic span")
            })
            .collect();
        store.ingest_batch(spans).expect("ingest");
        store.flush().expect("flush");
        store.pin_for_object_archive("multipart").expect("pin");
        fixture.pin = store.pin_path("multipart");
        let largest = fs::read_dir(&fixture.pin)
            .expect("pin files")
            .map(|entry| entry.expect("entry").metadata().expect("metadata").len())
            .max()
            .expect("nonempty pin");
        assert!(
            largest > PACK_TARGET_BYTES,
            "fixture must force multipart: {largest} bytes"
        );
        fixture
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn multipart_part_error_timeout_and_completion_error_abort_without_publication() {
    let fixture = Fixture::new();
    for failure in [
        Failure::SecondPartError,
        Failure::SecondPartTimeout,
        Failure::CompletionError,
    ] {
        let backing = Arc::new(InMemory::new());
        let counts = Arc::new(Counts::default());
        let wrapped = Arc::new(MultipartFaultStore {
            inner: Arc::clone(&backing),
            counts: Arc::clone(&counts),
            failure,
        });
        let mut options = RemoteOptions::new(Backend::Custom(wrapped));
        options.op_timeout = Duration::from_millis(100);
        let remote = Remote::open(options).expect("remote");
        let started = Instant::now();
        let error = remote
            .publish_pin(&fixture.pin, "failed")
            .expect_err("fault must fail publish");
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_secs(8), "{failure:?}: {error}");
        match failure {
            Failure::SecondPartTimeout => {
                assert!(error.to_string().contains("put_part timed out"), "{error}")
            }
            _ => assert!(
                error.to_string().contains("injected multipart failure"),
                "{error}"
            ),
        }
        let load = |counter: &AtomicUsize| counter.load(Ordering::SeqCst);
        assert_eq!(load(&counts.created), 1, "{failure:?}: multipart creation");
        assert!(
            load(&counts.part_calls) >= 2,
            "{failure:?}: second part reached"
        );
        assert!(
            load(&counts.parts_completed) >= 1,
            "{failure:?}: first part stored"
        );
        assert!(load(&counts.bytes_submitted) as u64 > PACK_TARGET_BYTES);
        assert_eq!(load(&counts.abort_calls), 1, "{failure:?}: abort invoked");
        assert_eq!(
            load(&counts.abort_completed),
            1,
            "{failure:?}: backing abort finished"
        );
        assert_eq!(
            load(&counts.complete_calls),
            usize::from(matches!(failure, Failure::CompletionError))
        );
        eprintln!(
            "{failure:?}: {} multipart, {} parts submitted, {} completed, {} bytes, {} abort, {:.3}s",
            load(&counts.created),
            load(&counts.part_calls),
            load(&counts.parts_completed),
            load(&counts.bytes_submitted),
            load(&counts.abort_completed),
            elapsed.as_secs_f64(),
        );
        let location = counts
            .location
            .lock()
            .expect("multipart location")
            .clone()
            .expect("created");
        for path in [location, Path::from("snapshots/failed/manifest.json")] {
            assert!(
                matches!(
                    block_on(backing.head(&path)),
                    Err(object_store::Error::NotFound { .. })
                ),
                "{failure:?}: failed upload or manifest became visible at {path}"
            );
        }
        assert!(
            remote.open_snapshot("failed").is_err(),
            "{failure:?}: snapshot visible"
        );
    }
}
