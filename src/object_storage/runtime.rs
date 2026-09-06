//! The synchronous adapter around the archive's async transport.
//!
//! The engine's read API is synchronous, so remote I/O must be driven to
//! completion from ordinary threads. The obvious `Runtime::block_on` is a
//! trap: called from inside any tokio worker (an embedding application's
//! runtime, a future refactor of the server) it panics rather than nests —
//! and so does `Runtime::drop`. So this adapter touches the runtime from
//! exactly one place: a **dedicated thread** it spawns at construction,
//! which builds a current-thread runtime, parks on a shutdown signal, and
//! is the thread the runtime is eventually dropped on. Callers only ever
//! `Handle::spawn` onto it and wait on a plain channel, which is safe from
//! any context — a caller inside an async context merely blocks its thread,
//! which is what a synchronous call means. Dropping the adapter from inside
//! an async context blocks that thread on a `JoinHandle::join`, never
//! panics; the embedded-runtime integration test proves construct → read →
//! drop inside a foreign tokio runtime.
//!
//! Concurrency is bounded by ADMISSION, not by thread count: every
//! operation takes the adapter's admission lock before it is spawned and
//! holds it until its result is back, so at most one remote operation is in
//! flight per [`super::Remote`] however many caller threads share it. (A
//! worker-thread count would not bound anything — async tasks interleave on
//! one worker.)

use std::sync::{mpsc, Mutex};

use super::{Error, Result};

/// One dedicated runtime thread, owned for the life of a [`super::Remote`].
pub(crate) struct SyncRuntime {
    handle: tokio::runtime::Handle,
    /// Serializes operations: held from before the spawn until the result
    /// is received. This is the concurrency bound.
    admission: Mutex<()>,
    /// Dropping this resolves the runtime thread's park future.
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SyncRuntime {
    pub(crate) fn new() -> std::io::Result<Self> {
        let (handle_sender, handle_receiver) = mpsc::channel();
        let (shutdown_sender, shutdown_receiver) = tokio::sync::oneshot::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("traza-object-io".to_owned())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = handle_sender.send(Err(error));
                        return;
                    }
                };
                let _ = handle_sender.send(Ok(runtime.handle().clone()));
                // Drive spawned work until the owner drops (or leaks) the
                // shutdown sender; then the runtime is dropped HERE, on its
                // own plain thread, never inside a caller's async context.
                runtime.block_on(async move {
                    let _ = shutdown_receiver.await;
                });
                // Timed-out requests may leave DNS work on Tokio's blocking
                // pool. Shutdown must not wait indefinitely for that work.
                runtime.shutdown_background();
            })?;
        let handle = handle_receiver
            .recv()
            .map_err(|_| std::io::Error::other("runtime thread died at startup"))??;
        Ok(Self {
            handle,
            admission: Mutex::new(()),
            shutdown: Some(shutdown_sender),
            thread: Some(thread),
        })
    }

    /// Runs `fut` to completion on the dedicated runtime and returns its
    /// result. Operations are admitted one at a time (see the module doc);
    /// the future must own its data (`'static`) because in the
    /// caller-panicked edge the task can outlive this call, with its result
    /// discarded.
    pub(crate) fn run<T: Send + 'static>(
        &self,
        fut: impl std::future::Future<Output = Result<T>> + Send + 'static,
    ) -> Result<T> {
        let _admitted = self
            .admission
            .lock()
            .map_err(|_| Error::Remote("remote admission lock poisoned".to_owned()))?;
        let (sender, receiver) = mpsc::channel();
        self.handle.spawn(async move {
            // A dropped receiver (caller thread panicked) makes this send
            // fail; nothing useful to do with the result then.
            let _ = sender.send(fut.await);
        });
        receiver
            .recv()
            .unwrap_or_else(|_| Err(Error::Remote("remote worker task panicked".to_owned())))
    }
}

impl Drop for SyncRuntime {
    fn drop(&mut self) {
        // Resolve the park, then wait for the runtime to be dropped on its
        // own thread. `join` blocks but never panics for being inside an
        // async context — the documented cost of dropping a `Remote` there
        // is a blocked thread for the shutdown's duration.
        drop(self.shutdown.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl std::fmt::Debug for SyncRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SyncRuntime")
    }
}
