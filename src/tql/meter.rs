//! The work one scan does, charged against the query deadline in
//! proportion to its cost.
//!
//! The deadline is consulted per unit of WORK, not per step of the run or
//! per path: one step can hold thousands of live paths, one path can fork
//! one environment per value of a list field, and one predicate can run a
//! regex of thousands of instructions over 64 KiB of text. Every place that
//! does work proportional to its input charges the meter, and the meter
//! checks the deadline every [`CHECK_EVERY`] units, so a scan stops within
//! a fraction of a millisecond of its budget wherever the work is.
//!
//! The meter also carries the scan's `dropped` flag: set wherever a cap
//! (environments per evaluation, live paths, remembered states) dropped an
//! alternative, so the result can say `inexact` rather than pass a partial
//! match set off as complete.

use crate::{Deadline, Result};

/// Units between two deadline checks. A unit is about one regex instruction
/// stepped over one character — a few nanoseconds — so a check falls every
/// fraction of a millisecond.
const CHECK_EVERY: u64 = 1 << 16;

/// Units charged for advancing one live path by one step of the run.
pub(crate) const PATH_UNITS: u64 = 32;

/// Units charged for one state a path reaches without consuming a step.
pub(crate) const STATE_UNITS: u64 = 4;

/// Units charged for forking one environment (a capture bound to a value).
pub(crate) const FORK_UNITS: u64 = 32;

/// Units charged for evaluating one condition, before the text it reads.
pub(crate) const CONDITION_UNITS: u64 = 8;

/// Units charged for reading one span as a step, before its catalog.
pub(crate) const SPAN_UNITS: u64 = 32;

/// Bytes of text read, hashed or parsed per unit.
pub(crate) const BYTES_PER_UNIT: u64 = 16;

/// The work meter of one scan.
pub(crate) struct Meter {
    deadline: Option<Deadline>,
    spent: u64,
    next_check: u64,
    /// Whether a cap dropped an alternative (an environment, a path, a
    /// remembered state), so a match may have been missed or reported later
    /// than the leftmost-first one.
    pub(crate) dropped: bool,
}

impl Meter {
    /// A meter charging against `deadline` (`None`: never refuses).
    pub(crate) fn new(deadline: Option<Deadline>) -> Self {
        Self {
            deadline,
            spent: 0,
            next_check: CHECK_EVERY,
            dropped: false,
        }
    }

    /// Charges `units` of work; errs with
    /// [`crate::Error::DeadlineExceeded`] once the deadline has passed,
    /// noticed within [`CHECK_EVERY`] units of it.
    pub(crate) fn charge(&mut self, units: u64) -> Result<()> {
        self.spent = self.spent.saturating_add(units);
        if self.spent >= self.next_check {
            self.next_check = self.spent.saturating_add(CHECK_EVERY);
            Deadline::check(self.deadline, 0)?;
        }
        Ok(())
    }

    /// Units charged so far.
    #[cfg(test)]
    pub(crate) fn spent(&self) -> u64 {
        self.spent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_deadline_is_checked_in_proportion_to_the_work() {
        let expired =
            Deadline::starting(std::time::Instant::now(), Some(std::time::Duration::ZERO));
        let mut meter = Meter::new(expired);
        // Below one check's worth of units, nothing is consulted.
        meter.charge(CHECK_EVERY - 1).expect("not yet checked");
        match meter.charge(1) {
            Err(crate::Error::DeadlineExceeded(_)) => {}
            other => panic!("expected a deadline refusal, got {other:?}"),
        }
        let mut free = Meter::new(None);
        free.charge(u64::MAX).expect("no deadline never refuses");
        free.charge(u64::MAX).expect("saturates");
        assert_eq!(free.spent(), u64::MAX);
    }
}
