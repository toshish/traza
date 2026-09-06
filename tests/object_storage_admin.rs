//! Separate-client publication and administration races, stopped at exact
//! backend calls rather than relying on sleeps or scheduler timing.
#![cfg(feature = "object-storage")]

use std::fmt;
use std::fs;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::stream::BoxStream;
use traza::object_storage::object_store::memory::InMemory;
use traza::object_storage::object_store::path::Path;
use traza::object_storage::object_store::{
    self, CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use traza::object_storage::testing::{block_on, Fault, FaultAction, FaultStore};
use traza::object_storage::{Backend, Error, Remote, RemoteOptions};
use traza::{Config, Durability, Store};

const TOMBSTONE: &str = "snapshots/snap/TOMBSTONE";
const INTENT: &str = "snapshots/snap/UPLOADING";
const MANIFEST: &str = "snapshots/snap/manifest.json";
const PACK: &str = "snapshots/snap/objects/00000000.pack";

struct Fixture {
    root: PathBuf,
    pin: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "traza-object-admin-{label}-{}-{nonce}",
            std::process::id()
        ));
        let store = Store::open(
            &root,
            Config {
                durability: Durability::Buffered,
                compaction: None,
                ..Config::default()
            },
        )
        .expect("fixture store");
        store
            .ingest_batch(vec![serde_json::from_value(serde_json::json!({
                "trace_id": "trace", "span_id": "span", "name": label, "service": "fixture",
                "start_time_ns": 1_000, "end_time_ns": 2_000,
            }))
            .unwrap()])
            .expect("fixture ingest");
        store
            .pin_for_object_archive("archive")
            .expect("fixture pin");
        let pin = store.pin_path("archive");
        Self { root, pin }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn remote(store: Arc<dyn ObjectStore>, identity: &str) -> Remote {
    let mut options = RemoteOptions::new(Backend::Custom(store));
    options.store_identity = identity.into();
    options.op_timeout = Duration::from_secs(10);
    Remote::open(options).expect("remote")
}

fn put_json(store: &dyn ObjectStore, path: &str, value: serde_json::Value) {
    block_on(store.put(
        &Path::from(path),
        serde_json::to_vec(&value).unwrap().into(),
    ))
    .expect("seed control");
}

fn read_json(store: &dyn ObjectStore, path: &str) -> serde_json::Value {
    let bytes = block_on(async { store.get(&Path::from(path)).await?.bytes().await })
        .expect("read control");
    serde_json::from_slice(&bytes).expect("control JSON")
}

fn exists(store: &dyn ObjectStore, path: &str) -> bool {
    match block_on(store.head(&Path::from(path))) {
        Ok(_) => true,
        Err(object_store::Error::NotFound { .. }) => false,
        Err(error) => panic!("unexpected HEAD: {error}"),
    }
}

#[derive(Clone, Copy, Debug)]
enum StopAt {
    PackRead,
    ManifestPut,
    TombstonePut,
}

/// Only the publisher/deleter under test uses this wrapper. Other clients
/// use the same backing store directly, so one stopped operation cannot
/// accidentally block the janitor or the competing client.
#[derive(Debug)]
struct PausedStore {
    inner: Arc<dyn ObjectStore>,
    stop_at: StopAt,
    gate: Mutex<Option<(mpsc::Sender<()>, tokio::sync::oneshot::Receiver<()>)>>,
}

impl PausedStore {
    fn new(inner: Arc<dyn ObjectStore>, stop_at: StopAt) -> (Arc<Self>, Pause) {
        let (entered, reached) = mpsc::channel();
        let (release, resume) = tokio::sync::oneshot::channel();
        (
            Arc::new(Self {
                inner,
                stop_at,
                gate: Mutex::new(Some((entered, resume))),
            }),
            Pause {
                reached,
                release: Some(release),
            },
        )
    }

    async fn stop_once(&self) {
        let gate = self.gate.lock().expect("gate lock").take();
        if let Some((entered, resume)) = gate {
            let _ = entered.send(());
            let _ = resume.await;
        }
    }
}

struct Pause {
    reached: mpsc::Receiver<()>,
    release: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Pause {
    fn wait(&self) {
        self.reached
            .recv_timeout(Duration::from_secs(10))
            .expect("backend pause reached");
    }
    fn resume(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
    }
}

impl Drop for Pause {
    fn drop(&mut self) {
        self.resume();
    }
}

/// Always release and join the owned client, including assertion failures.
struct PausedTask<T> {
    pause: Pause,
    task: Option<std::thread::JoinHandle<T>>,
}

impl<T: Send + 'static> PausedTask<T> {
    fn spawn(pause: Pause, run: impl FnOnce() -> T + Send + 'static) -> Self {
        Self {
            pause,
            task: Some(std::thread::spawn(run)),
        }
    }

    fn wait(&self) {
        self.pause.wait();
    }

    fn finish(mut self) -> T {
        self.pause.resume();
        self.task.take().unwrap().join().expect("client thread")
    }
}

impl<T> Drop for PausedTask<T> {
    fn drop(&mut self) {
        self.pause.resume();
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

impl fmt::Display for PausedStore {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "PausedStore")
    }
}

#[async_trait::async_trait]
impl ObjectStore for PausedStore {
    async fn put_opts(
        &self,
        path: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<PutResult> {
        let stop = match self.stop_at {
            StopAt::ManifestPut => path.as_ref() == MANIFEST,
            StopAt::TombstonePut => path.as_ref() == TOMBSTONE,
            StopAt::PackRead => false,
        };
        if stop {
            self.stop_once().await;
        }
        self.inner.put_opts(path, payload, options).await
    }

    async fn get_opts(&self, path: &Path, options: GetOptions) -> object_store::Result<GetResult> {
        let stop =
            matches!(self.stop_at, StopAt::PackRead) && !options.head && path.as_ref() == PACK;
        let result = self.inner.get_opts(path, options).await?;
        // Retain the already-read immutable object while cleanup removes
        // its key. After resume, the publisher can complete verification.
        if stop {
            self.stop_once().await;
        }
        Ok(result)
    }

    async fn put_multipart_opts(
        &self,
        path: &Path,
        options: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(path, options).await
    }

    fn delete_stream(
        &self,
        paths: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(paths)
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

#[test]
fn cleanup_permanently_retires_an_id_before_a_publisher_resumes() {
    let source = Fixture::new("old");
    let replacement = Fixture::new("replacement");
    let backing = Arc::new(InMemory::new());
    let (paused, pause) = PausedStore::new(backing.clone(), StopAt::PackRead);
    let publisher = remote(paused, "owner");
    let janitor = remote(backing.clone(), "owner");
    let contender = remote(backing.clone(), "owner");
    let pin = source.pin.clone();
    let work = PausedTask::spawn(pause, move || publisher.publish_pin(&pin, "snap"));
    work.wait();
    // Deliberately resume an old writer after the acknowledged cleanup:
    // the durable fence must survive even this violated precondition.
    janitor.cleanup_snapshot("snap", true).expect("cleanup");
    let competing = contender.publish_pin(&replacement.pin, "snap");
    let old = work.finish();
    assert!(matches!(competing, Err(Error::Refused(_))), "{competing:?}");
    assert!(matches!(old, Err(Error::Refused(_))), "{old:?}");
    assert_eq!(read_json(backing.as_ref(), TOMBSTONE)["store"], "owner");
    assert!(!exists(backing.as_ref(), MANIFEST));
    assert!(matches!(
        contender.open_snapshot("snap"),
        Err(Error::NotFound(_))
    ));
    contender
        .publish_pin(&replacement.pin, "new-id")
        .expect("new ID works");
}

#[test]
fn a_manifest_put_delayed_past_cleanup_cannot_report_success_or_be_opened() {
    let source = Fixture::new("late-commit");
    let backing = Arc::new(InMemory::new());
    let (paused, pause) = PausedStore::new(backing.clone(), StopAt::ManifestPut);
    let publisher = remote(paused, "owner");
    let janitor = remote(backing.clone(), "owner");
    let reader = remote(backing.clone(), "owner");
    let pin = source.pin.clone();
    let work = PausedTask::spawn(pause, move || publisher.publish_pin(&pin, "snap"));
    work.wait();
    janitor
        .cleanup_snapshot("snap", true)
        .expect("cleanup after precommit check");
    let result = work.finish();
    assert!(matches!(result, Err(Error::Refused(_))), "{result:?}");
    assert!(matches!(
        reader.open_snapshot("snap"),
        Err(Error::NotFound(_))
    ));
    // A late write may leave invisible debris. Resume removes it without
    // reopening or reusing the retired name.
    janitor
        .cleanup_snapshot("snap", true)
        .expect("resume cleanup");
    assert!(!exists(backing.as_ref(), MANIFEST));
    assert!(exists(backing.as_ref(), TOMBSTONE));
}

#[test]
fn unscoped_delete_preserves_the_owner_through_an_interrupted_sweep() {
    let source = Fixture::new("delete-owner");
    let backing = Arc::new(InMemory::new());
    let owner = remote(backing.clone(), "owner");
    owner.publish_pin(&source.pin, "snap").expect("publish");
    let fault = Arc::new(FaultStore::new(backing.clone()));
    fault.add_fault(Fault {
        op: "delete",
        substring: ".pack".into(),
        remaining: 1,
        action: FaultAction::Fail,
    });
    let unscoped = remote(fault, "");
    assert!(unscoped.delete_snapshot("snap").is_err());
    assert_eq!(read_json(backing.as_ref(), TOMBSTONE)["store"], "owner");
    let foreign = remote(backing.clone(), "foreign");
    assert!(matches!(
        foreign.delete_snapshot("snap"),
        Err(Error::Refused(_))
    ));
    assert!(matches!(
        foreign.cleanup_snapshot("snap", true),
        Err(Error::Refused(_))
    ));
    assert!(exists(backing.as_ref(), PACK));
    owner.delete_snapshot("snap").expect("owner resumes");
    assert!(!exists(backing.as_ref(), PACK));
}

#[test]
fn unscoped_cleanup_preserves_the_upload_owner_through_an_interrupted_sweep() {
    let backing = Arc::new(InMemory::new());
    put_json(
        backing.as_ref(),
        INTENT,
        serde_json::json!({"snapshot": "snap", "store": "owner"}),
    );
    put_json(backing.as_ref(), PACK, serde_json::json!("unfinished pack"));
    let fault = Arc::new(FaultStore::new(backing.clone()));
    fault.add_fault(Fault {
        op: "delete",
        substring: ".pack".into(),
        remaining: 1,
        action: FaultAction::Fail,
    });
    assert!(remote(fault, "").cleanup_snapshot("snap", true).is_err());
    assert_eq!(read_json(backing.as_ref(), TOMBSTONE)["store"], "owner");
    assert!(matches!(
        remote(backing.clone(), "foreign").cleanup_snapshot("snap", true),
        Err(Error::Refused(_))
    ));
    assert!(exists(backing.as_ref(), PACK));
    remote(backing.clone(), "owner")
        .cleanup_snapshot("snap", true)
        .expect("owner resumes");
    assert!(!exists(backing.as_ref(), PACK));
    assert!(exists(backing.as_ref(), TOMBSTONE));
}

#[test]
fn a_losing_tombstone_create_never_overwrites_identity_and_checks_the_winner() {
    let source = Fixture::new("tombstone-race");
    for winner in ["owner", "foreign"] {
        let backing = Arc::new(InMemory::new());
        remote(backing.clone(), "owner")
            .publish_pin(&source.pin, "snap")
            .expect("publish");
        let (paused, pause) = PausedStore::new(backing.clone(), StopAt::TombstonePut);
        let deleter = remote(paused, "owner");
        let work = PausedTask::spawn(pause, move || deleter.delete_snapshot("snap"));
        work.wait();
        // Install the competing writer's exact control record between the
        // absent read and conditional create. Foreign evidence must refuse
        // the sweep; matching evidence permits an idempotent resume.
        let winning = serde_json::json!({"snapshot": "snap", "store": winner,
            "deleted_unix_ns": 7});
        put_json(backing.as_ref(), TOMBSTONE, winning.clone());
        let result = work.finish();
        assert_eq!(read_json(backing.as_ref(), TOMBSTONE), winning);
        if winner == "owner" {
            result.expect("matching winner permits resume");
            assert!(!exists(backing.as_ref(), PACK));
        } else {
            assert!(matches!(result, Err(Error::Refused(_))), "{result:?}");
            assert!(exists(backing.as_ref(), PACK));
            assert!(exists(backing.as_ref(), MANIFEST));
        }
    }
}

#[test]
fn configured_clients_reject_missing_empty_and_malformed_recorded_identities() {
    for control in [TOMBSTONE, INTENT] {
        for identity in [
            None,
            Some(serde_json::json!("")),
            Some(serde_json::Value::Null),
            Some(serde_json::json!(7)),
        ] {
            let backing = Arc::new(InMemory::new());
            let mut value = serde_json::json!({"snapshot": "snap"});
            if let Some(identity) = identity {
                value["store"] = identity;
            }
            put_json(backing.as_ref(), control, value.clone());
            put_json(backing.as_ref(), PACK, serde_json::json!("preserve me"));
            let client = remote(backing.clone(), "owner");
            let result = client.cleanup_snapshot("snap", true);
            assert!(
                matches!(result, Err(Error::Refused(_))),
                "{control}: {result:?}"
            );
            assert_eq!(read_json(backing.as_ref(), control), value);
            assert!(exists(backing.as_ref(), PACK));
            if control == INTENT {
                assert!(!exists(backing.as_ref(), TOMBSTONE));
            }
        }
    }
}

#[test]
fn configured_clients_cannot_adopt_an_unidentified_published_snapshot() {
    let source = Fixture::new("unknown-owner");
    let backing = Arc::new(InMemory::new());
    let unscoped = remote(backing.clone(), "");
    unscoped
        .publish_pin(&source.pin, "snap")
        .expect("unscoped publish");
    unscoped
        .open_snapshot("snap")
        .expect("unscoped read remains allowed");
    let configured = remote(backing.clone(), "owner");
    assert!(matches!(
        configured.open_snapshot("snap"),
        Err(Error::Refused(_))
    ));
    assert!(matches!(
        configured.inspect_snapshot("snap"),
        Err(Error::Refused(_))
    ));
    assert!(matches!(
        configured.verify_snapshot("snap", false),
        Err(Error::Refused(_))
    ));
    assert!(matches!(
        configured.delete_snapshot("snap"),
        Err(Error::Refused(_))
    ));
    assert!(matches!(
        configured.cleanup_snapshot("snap", false),
        Err(Error::Refused(_))
    ));
    assert!(exists(backing.as_ref(), MANIFEST));
    assert!(!exists(backing.as_ref(), TOMBSTONE));
}
