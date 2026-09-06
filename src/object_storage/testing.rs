//! Injectable test backends for the archive: a programmable fault wrapper
//! around any [`ObjectStore`].
//!
//! This ships in the feature (not behind `cfg(test)`) so integration tests —
//! this crate's and an embedder's — can exercise the archive's failure
//! contracts without their own `object_store` dependency: wrap an
//! [`object_store::memory::InMemory`], script faults, and hand the wrapper
//! in through [`super::Backend::Custom`]. Nothing here is reachable from a
//! production configuration unless an operator deliberately constructs it.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::stream::BoxStream;
use futures_util::StreamExt;
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult,
};

/// Drives one future to completion on a throwaway current-thread runtime.
///
/// For tests that must speak to an [`ObjectStore`] directly — tampering with
/// published objects, planting markers — without carrying their own async
/// runtime dependency. Not for production paths, which go through the
/// bounded [`super::Remote`] adapter.
pub fn block_on<T>(fut: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(fut)
}

/// What a matched fault does to the operation.
#[derive(Clone, Debug)]
pub enum FaultAction {
    /// Return a generic store error instead of performing the operation.
    Fail,
    /// Sleep this long, then perform the operation — for timeout tests.
    Delay(Duration),
}

/// One scripted fault: fires on operations of `op` whose path contains
/// `substring`, up to `remaining` times.
#[derive(Clone, Debug)]
pub struct Fault {
    /// Operation kind: `"put"`, `"get"`, `"head"`, `"delete"`, `"list"`,
    /// `"multipart"`, `"copy"`. `head` is a `get` with the head option, as
    /// the transport itself defines it.
    pub op: &'static str,
    /// Path substring the fault matches.
    pub substring: String,
    /// How many more times it fires.
    pub remaining: usize,
    /// What it does.
    pub action: FaultAction,
}

#[derive(Default)]
struct State {
    faults: Vec<Fault>,
    log: VecDeque<String>,
}

/// A delegating [`ObjectStore`] with scripted faults and an operation log.
///
/// The log records every operation in order (`"put snapshots/x/objects/…"`),
/// which is how tests assert publication ordering — the manifest put must be
/// the LAST put of a successful publish.
pub struct FaultStore {
    inner: Arc<dyn ObjectStore>,
    state: Arc<Mutex<State>>,
}

impl FaultStore {
    /// Wraps `inner` with an empty fault script.
    pub fn new(inner: Arc<dyn ObjectStore>) -> Self {
        Self {
            inner,
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    /// Adds a fault to the script.
    pub fn add_fault(&self, fault: Fault) {
        if let Ok(mut state) = self.state.lock() {
            state.faults.push(fault);
        }
    }

    /// The operations seen so far, in order.
    pub fn operations(&self) -> Vec<String> {
        self.state
            .lock()
            .map(|state| state.log.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Records the operation and returns the matched action, if any.
    fn intercept(state: &Mutex<State>, op: &'static str, location: &Path) -> Option<FaultAction> {
        let mut state = state.lock().ok()?;
        state.log.push_back(format!("{op} {}", location.as_ref()));
        let position = state.faults.iter().position(|fault| {
            fault.op == op && fault.remaining > 0 && location.as_ref().contains(&fault.substring)
        })?;
        state.faults[position].remaining -= 1;
        Some(state.faults[position].action.clone())
    }

    async fn apply(
        state: &Mutex<State>,
        op: &'static str,
        location: &Path,
    ) -> Result<(), object_store::Error> {
        match Self::intercept(state, op, location) {
            None => Ok(()),
            Some(FaultAction::Delay(pause)) => {
                tokio::time::sleep(pause).await;
                Ok(())
            }
            Some(FaultAction::Fail) => Err(object_store::Error::Generic {
                store: "fault",
                source: format!("scripted {op} fault at {}", location.as_ref()).into(),
            }),
        }
    }
}

impl fmt::Display for FaultStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FaultStore({})", self.inner)
    }
}

impl fmt::Debug for FaultStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FaultStore")
    }
}

#[async_trait::async_trait]
impl ObjectStore for FaultStore {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> Result<PutResult, object_store::Error> {
        Self::apply(&self.state, "put", location).await?;
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        opts: PutMultipartOptions,
    ) -> Result<Box<dyn MultipartUpload>, object_store::Error> {
        Self::apply(&self.state, "multipart", location).await?;
        self.inner.put_multipart_opts(location, opts).await
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> Result<GetResult, object_store::Error> {
        // A head is a get with the head option — logged and faulted under
        // its own name so tests can target existence probes separately from
        // data reads.
        let op = if options.head { "head" } else { "get" };
        Self::apply(&self.state, op, location).await?;
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, Result<Path, object_store::Error>>,
    ) -> BoxStream<'static, Result<Path, object_store::Error>> {
        // Deletes flow through here one path at a time (the `delete`
        // convenience wraps a one-element stream), so per-path faults are
        // applied as each element is processed.
        let inner = Arc::clone(&self.inner);
        let state = Arc::clone(&self.state);
        locations
            .then(move |location| {
                let inner = Arc::clone(&inner);
                let state = Arc::clone(&state);
                async move {
                    let location = location?;
                    Self::apply(&state, "delete", &location).await?;
                    inner.delete(&location).await?;
                    Ok(location)
                }
            })
            .boxed()
    }

    fn list(
        &self,
        prefix: Option<&Path>,
    ) -> BoxStream<'static, Result<ObjectMeta, object_store::Error>> {
        if let Ok(mut state) = self.state.lock() {
            state.log.push_back(format!(
                "list {}",
                prefix.map(|p| p.as_ref().to_owned()).unwrap_or_default()
            ));
        }
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&Path>,
    ) -> Result<ListResult, object_store::Error> {
        if let Ok(mut state) = self.state.lock() {
            state.log.push_back(format!(
                "list_with_delimiter {}",
                prefix.map(|p| p.as_ref().to_owned()).unwrap_or_default()
            ));
        }
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: CopyOptions,
    ) -> Result<(), object_store::Error> {
        Self::apply(&self.state, "copy", from).await?;
        self.inner.copy_opts(from, to, options).await
    }
}
