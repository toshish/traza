//! The two points where a feature may refuse or observe ingest.
//!
//! [`Store::ingest_batch`] calls [`Store::admission_check`] once the batch is
//! known valid and before anything is decided about it — before the erasure
//! gate, the payload offload, or the write-ahead log. A refusal there is
//! batch-atomic and leaves no trace: nothing was written, nothing was
//! acknowledged.
//!
//! The private `Store::admit` calls [`Store::observe_admitted`] once a batch is
//! ACKNOWLEDGED — durable under the configured mode and already published to
//! the live tail — so an observer sees exactly what the store accepted, never
//! a batch whose ingest then failed. Whatever it reports rides back to the
//! caller on [`crate::Admission`].
//!
//! Both hooks are deliberately empty in this build. They exist as named seams
//! so policy (quotas, pauses, admission caps) and observation (run guards,
//! usage counters) are added here rather than threaded through the admission
//! path's lock-ordering argument.

use std::sync::Arc;

use crate::{guard, Result, Span, Store};

/// What observing an admitted batch produced.
#[derive(Debug, Default)]
pub(crate) struct Observed {
    /// Run-guard signals for runs in the batch, copied to
    /// [`crate::Admission::signals`].
    pub(crate) fuses: Vec<guard::Signal>,
}

impl Store {
    /// Refuses a batch before admission, or lets it through.
    ///
    /// Runs with NO engine lock held, so it may take its own leaf locks, but it
    /// is on every ingest's path: it must be O(batch) and must not do I/O that
    /// can block on another request. An `Err` here fails the whole batch
    /// before any of it is written.
    pub(crate) fn admission_check(&self, _spans: &[Span]) -> Result<()> {
        Ok(())
    }

    /// Observes a batch the store has just acknowledged.
    ///
    /// Runs after the batch is durable and published to the tail, with no
    /// engine lock held. It cannot fail the ingest — the batch is already
    /// acknowledged — so it returns what it saw rather than a `Result`, and it
    /// must be non-blocking and O(batch): every ingest waits for it before
    /// its response is written.
    pub(crate) fn observe_admitted(&self, _spans: &[Arc<Span>]) -> Observed {
        Observed::default()
    }
}
