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
//! They exist as named seams so policy and observation are added here rather
//! than threaded through the admission path's lock-ordering argument. The
//! admission check enforces the admission caps ([`crate::caps`]) and then the
//! tenant quotas and pauses ([`crate::usage`]); what it measured on the way
//! rides to the acknowledgement as [`crate::caps::Admitting`], where usage is
//! charged.

use std::sync::Arc;

use crate::{caps, guard, Result, Span, Store};

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
    ///
    /// Caps first, then quotas: a malformed batch is the client's defect
    /// whatever the tenant's quota says, and must not be counted as a quota
    /// rejection. The `Ok` carries each span's measured size for the usage
    /// charge at acknowledgement.
    pub(crate) fn admission_check(&self, spans: &[Span]) -> Result<caps::Admitting> {
        let admitting = caps::measure(&self.config, spans)?;
        self.platform.usage.check(
            &self.platform.limits,
            spans,
            &admitting,
            crate::unix_now_ns(),
        )?;
        Ok(admitting)
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
