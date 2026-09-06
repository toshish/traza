//! Adversarial response streams through the public archive API.
#![cfg(feature = "object-storage")]

use std::fmt;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::stream::{self, BoxStream};
use futures_util::StreamExt;
use traza::object_storage::object_store::memory::InMemory;
use traza::object_storage::object_store::path::Path;
use traza::object_storage::object_store::{
    CopyOptions, GetOptions, GetResult, GetResultPayload, ListResult, MultipartUpload, ObjectMeta,
    ObjectStore, ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use traza::object_storage::testing::block_on;
use traza::object_storage::{Backend, Error, Remote, RemoteOptions};
use traza::{Config, Durability, Store};

type StoreError = traza::object_storage::object_store::Error;

#[derive(Clone, Copy, Debug)]
enum Mode {
    OversizedRange,
    WrongRange,
    SlowControl,
    SlowVerify,
    SlowList,
    FloodList,
}

struct StreamBackend {
    inner: Arc<InMemory>,
    mode: Mode,
    polls: Arc<AtomicUsize>,
    listings: AtomicUsize,
    template: ObjectMeta,
}

impl StreamBackend {
    fn new(inner: Arc<InMemory>, mode: Mode) -> Arc<Self> {
        let seed = Path::from("template-outside-archive-prefix");
        block_on(inner.put(&seed, vec![1u8].into())).unwrap();
        let template = block_on(inner.head(&seed)).unwrap();
        Arc::new(Self {
            inner,
            mode,
            polls: Arc::new(AtomicUsize::new(0)),
            listings: AtomicUsize::new(0),
            template,
        })
    }
}

impl fmt::Debug for StreamBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("StreamBackend")
            .field(&self.mode)
            .finish()
    }
}

impl fmt::Display for StreamBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "StreamBackend({:?})", self.mode)
    }
}

#[async_trait::async_trait]
impl ObjectStore for StreamBackend {
    async fn put_opts(
        &self,
        path: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> Result<PutResult, StoreError> {
        self.inner.put_opts(path, payload, options).await
    }

    async fn put_multipart_opts(
        &self,
        path: &Path,
        options: PutMultipartOptions,
    ) -> Result<Box<dyn MultipartUpload>, StoreError> {
        self.inner.put_multipart_opts(path, options).await
    }

    async fn get_opts(&self, path: &Path, options: GetOptions) -> Result<GetResult, StoreError> {
        let ranged = options.range.is_some();
        let head = options.head;
        let mut response = self.inner.get_opts(path, options).await?;
        if head {
            return Ok(response);
        }
        let polls = Arc::clone(&self.polls);
        if ranged && matches!(self.mode, Mode::OversizedRange) {
            let oversized = vec![0u8; (response.range.end - response.range.start) as usize + 1];
            response.payload = GetResultPayload::Stream(
                stream::iter(vec![
                    Ok(oversized.into()),
                    Err(StoreError::Generic {
                        store: "transfer-test",
                        source: "oversized response tail was polled".into(),
                    }),
                ])
                .inspect(move |_| {
                    polls.fetch_add(1, Ordering::SeqCst);
                })
                .boxed(),
            );
        } else if ranged && matches!(self.mode, Mode::WrongRange) {
            response.range.start += 1;
            response.range.end += 1;
            let GetResultPayload::Stream(body) = response.payload;
            response.payload = GetResultPayload::Stream(
                body.inspect(move |_| {
                    polls.fetch_add(1, Ordering::SeqCst);
                })
                .boxed(),
            );
        } else if !ranged
            && (matches!(self.mode, Mode::SlowControl) && path.as_ref().ends_with("/TOMBSTONE")
                || matches!(self.mode, Mode::SlowVerify) && path.as_ref().contains("/objects/"))
        {
            let payload = std::mem::replace(
                &mut response.payload,
                GetResultPayload::Stream(stream::empty().boxed()),
            );
            let GetResultPayload::Stream(mut body) = payload;
            let mut bytes = Vec::new();
            while let Some(chunk) = body.next().await {
                bytes.extend_from_slice(&chunk?);
            }
            assert!(
                bytes.len() < 1 << 20,
                "fixture must have a 1.3s overall budget"
            );
            let started = tokio::time::Instant::now();
            response.payload = GetResultPayload::Stream(
                stream::unfold((0usize, Some(bytes)), move |(step, mut bytes)| {
                    let polls = Arc::clone(&polls);
                    async move {
                        polls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep_until(
                            started + Duration::from_millis((step as u64 + 1) * 200),
                        )
                        .await;
                        if step == 6 {
                            return None;
                        }
                        Some((
                            Ok(if step == 5 {
                                bytes.take().unwrap()
                            } else {
                                Vec::new()
                            }
                            .into()),
                            (step + 1, bytes),
                        ))
                    }
                })
                .boxed(),
            );
        }
        Ok(response)
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, Result<Path, StoreError>>,
    ) -> BoxStream<'static, Result<Path, StoreError>> {
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta, StoreError>> {
        let call = self.listings.fetch_add(1, Ordering::SeqCst);
        let prefix = prefix.cloned().unwrap_or_default();
        let mut template = self.template.clone();
        let polls = Arc::clone(&self.polls);
        if matches!(self.mode, Mode::FloodList) {
            return stream::iter(0..250_000)
                .map(move |index| {
                    polls.fetch_add(1, Ordering::SeqCst);
                    let mut meta = template.clone();
                    meta.location = prefix.clone().join("flood").join(format!("object-{index}"));
                    Ok(meta)
                })
                .boxed();
        }
        if matches!(self.mode, Mode::SlowList) && call == 0 {
            // A delete inventory may contain the permanent tombstone; all
            // repetitions are harmless if the old collector accepts late EOF.
            template.location = prefix.join("TOMBSTONE");
            let started = tokio::time::Instant::now();
            return stream::unfold(0usize, move |step| {
                let polls = Arc::clone(&polls);
                let template = template.clone();
                async move {
                    polls.fetch_add(1, Ordering::SeqCst);
                    let millis = match step {
                        19 => 3_120,
                        20 => 3_280,
                        _ => (step as u64 + 1) * 160,
                    };
                    tokio::time::sleep_until(started + Duration::from_millis(millis)).await;
                    if step == 20 {
                        None
                    } else {
                        Some((Ok(template), step + 1))
                    }
                }
            })
            .boxed();
        }
        self.inner.list(Some(&prefix))
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> Result<ListResult, StoreError> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: CopyOptions,
    ) -> Result<(), StoreError> {
        self.inner.copy_opts(from, to, options).await
    }
}

struct Source {
    directory: PathBuf,
    pin: PathBuf,
}

impl Drop for Source {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn source() -> Source {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory =
        std::env::temp_dir().join(format!("traza-transfer-{}-{unique}", std::process::id()));
    let store = Store::open(
        &directory,
        Config {
            durability: Durability::Buffered,
            compaction: None,
            ..Config::default()
        },
    )
    .unwrap();
    let span = serde_json::from_value(serde_json::json!({
        "trace_id": "trace", "span_id": "span", "name": "operation", "service": "test",
        "start_time_ns": 10, "end_time_ns": 20, "attributes": {"answer": 42}
    }))
    .unwrap();
    store.ingest_batch(vec![span]).unwrap();
    store.flush().unwrap();
    store.pin_for_object_archive("archive").unwrap();
    let pin = store.pin_path("archive");
    drop(store);
    Source { directory, pin }
}

fn remote(backend: Arc<dyn ObjectStore>, timeout: Duration) -> Remote {
    let mut options = RemoteOptions::new(Backend::Custom(backend));
    options.op_timeout = timeout;
    Remote::open(options).unwrap()
}

fn published() -> (Source, Arc<InMemory>) {
    let source = source();
    let inner = Arc::new(InMemory::new());
    remote(inner.clone(), Duration::from_secs(5))
        .publish_pin(&source.pin, "snap")
        .unwrap();
    (source, inner)
}

fn assert_timeout(error: Error) {
    assert!(
        matches!(error, Error::Remote(_)),
        "expected transport timeout, got {error:?}"
    );
    let text = error.to_string();
    assert!(
        text.contains("timed out") || text.contains("overall"),
        "{text}"
    );
}

#[test]
fn oversized_range_is_rejected_without_polling_its_tail() {
    let (_source, inner) = published();
    let backend = StreamBackend::new(inner, Mode::OversizedRange);
    let reader = remote(backend.clone(), Duration::from_secs(2));
    let error = reader.open_snapshot("snap").unwrap_err();
    assert!(matches!(error, Error::Corrupt(_)), "{error:?}");
    assert_eq!(
        backend.polls.load(Ordering::SeqCst),
        1,
        "oversized tail must never be polled"
    );
}

#[test]
fn incorrect_response_range_is_rejected_before_body_polling() {
    let (_source, inner) = published();
    let backend = StreamBackend::new(inner, Mode::WrongRange);
    let error = remote(backend.clone(), Duration::from_secs(2))
        .open_snapshot("snap")
        .unwrap_err();
    assert!(matches!(error, Error::Corrupt(_)), "{error:?}");
    assert_eq!(backend.polls.load(Ordering::SeqCst), 0);
}

#[test]
fn control_object_eof_after_overall_deadline_is_not_accepted() {
    let inner = Arc::new(InMemory::new());
    block_on(inner.put(
        &Path::from("snapshots/snap/TOMBSTONE"),
        br#"{"store":""}"#.to_vec().into(),
    ))
    .unwrap();
    let backend = StreamBackend::new(inner, Mode::SlowControl);
    let error = remote(backend.clone(), Duration::from_millis(300))
        .inspect_snapshot("snap")
        .unwrap_err();
    assert_timeout(error);
    assert!(
        backend.polls.load(Ordering::SeqCst) >= 6,
        "must reach the overall deadline, not fail on inactivity"
    );
}

#[test]
fn verification_eof_after_overall_deadline_cannot_publish() {
    let source = source();
    let inner = Arc::new(InMemory::new());
    let backend = StreamBackend::new(inner.clone(), Mode::SlowVerify);
    let error = remote(backend.clone(), Duration::from_millis(300))
        .publish_pin(&source.pin, "snap")
        .unwrap_err();
    assert_timeout(error);
    assert!(backend.polls.load(Ordering::SeqCst) >= 6);
    assert!(
        block_on(inner.head(&Path::from("snapshots/snap/manifest.json"))).is_err(),
        "failed verification must leave no visible manifest"
    );
}

#[test]
fn inventory_eof_after_overall_deadline_cannot_finish_deletion() {
    let inner = Arc::new(InMemory::new());
    block_on(inner.put(
        &Path::from("snapshots/snap/TOMBSTONE"),
        br#"{"store":""}"#.to_vec().into(),
    ))
    .unwrap();
    let backend = StreamBackend::new(inner, Mode::SlowList);
    let error = remote(backend.clone(), Duration::from_millis(200))
        .delete_snapshot("snap")
        .unwrap_err();
    assert_timeout(error);
    assert!(backend.polls.load(Ordering::SeqCst) >= 20);
}

#[test]
fn snapshot_directory_inventory_stops_at_the_object_cap() {
    let backend = StreamBackend::new(Arc::new(InMemory::new()), Mode::FloodList);
    let error = remote(backend.clone(), Duration::from_secs(5))
        .list_snapshots()
        .unwrap_err();
    assert!(matches!(error, Error::Remote(_)), "{error:?}");
    assert!(error.to_string().contains("more than 200016"), "{error}");
    assert_eq!(backend.polls.load(Ordering::SeqCst), 200_017);
}
