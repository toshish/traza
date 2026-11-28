//! The repeat window: how often one step fingerprint occurs among a run's
//! latest steps.
//!
//! `max_repeats` counts a fingerprint (the same call with the same input)
//! over the run's last `repeat_window_steps` steps, in step order — not
//! over the whole run. A loop repeats within a few steps of itself; a long
//! interactive session that takes the same screenshot once an hour for three
//! days spreads its repeats over thousands of steps, and is not looping.
//!
//! A run keeps its latest [`MAX_REPEAT_WINDOW_STEPS`] counted steps here
//! ([`RecentSteps`]): enough to answer for any window a policy may name,
//! whatever window the policy asks for at the moment it is asked. Steps that
//! arrive in step order append in O(1); anything else — a span arriving
//! with an earlier start than the latest, a re-sent span that moved, was
//! renamed or stopped being a step — is settled by rebuilding from the run's
//! marks once its fold is over.
//!
//! # Where a stop latches
//!
//! A replay of the store folds a run's steps in step order, so it latches
//! at the first step whose window reaches the limit: it latches exactly when
//! the run's [`peak`] — the most occurrences of one fingerprint inside any
//! window of its steps — reaches the limit. A live fold of appends sees the
//! same windows the replay does as they form, and checks each; a fold that
//! changed the run other than by appending checks the peak over the whole
//! run once it is over. Either way a live run stops exactly when a rebuild
//! of what the store holds would.

use std::collections::{HashMap, VecDeque};

use super::policy::MAX_REPEAT_WINDOW_STEPS;
use crate::steer::handoff::Position;

/// One counted step: where it falls in step order, and its fingerprint key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Slot {
    /// `(start, span key)`, the handoff tracker's step order.
    pub(crate) at: Position,
    /// The step's fingerprint key (never zero: an unfingerprinted step is
    /// not counted).
    pub(crate) fingerprint: u64,
}

/// A run's latest counted steps, ascending by position, at most
/// [`MAX_REPEAT_WINDOW_STEPS`] of them.
#[derive(Debug, Default)]
pub(crate) struct RecentSteps {
    slots: VecDeque<Slot>,
}

impl RecentSteps {
    /// Appends `slot` when it falls after every slot held, dropping the
    /// oldest past the cap; `false`, changing nothing, when it does not.
    pub(crate) fn append(&mut self, slot: Slot) -> bool {
        if self.slots.back().is_some_and(|last| slot.at <= last.at) {
            return false;
        }
        self.slots.push_back(slot);
        if self.slots.len() > MAX_REPEAT_WINDOW_STEPS as usize {
            self.slots.pop_front();
        }
        true
    }

    /// Replaces the slots with the latest of `sorted` (ascending).
    pub(crate) fn rebuild(&mut self, sorted: &[Slot]) {
        let keep = sorted.len().min(MAX_REPEAT_WINDOW_STEPS as usize);
        self.slots.clear();
        self.slots.extend(&sorted[sorted.len() - keep..]);
    }

    pub(crate) fn clear(&mut self) {
        self.slots.clear();
    }

    /// The most frequent fingerprint among the last `window` slots, with its
    /// count; among equally frequent ones, the one that occurred latest.
    pub(crate) fn top(&self, window: usize) -> Option<(u64, u64)> {
        // fingerprint → (count, newest-first index of its latest occurrence)
        let mut counts: HashMap<u64, (u64, usize)> = HashMap::new();
        for (index, slot) in self.slots.iter().rev().take(window).enumerate() {
            counts.entry(slot.fingerprint).or_insert((0, index)).0 += 1;
        }
        counts
            .into_iter()
            .max_by(|(_, (left, left_at)), (_, (right, right_at))| {
                left.cmp(right).then(right_at.cmp(left_at))
            })
            .map(|(fingerprint, (count, _))| (fingerprint, count))
    }
}

/// The most occurrences of one fingerprint inside any `window` consecutive
/// slots of `sorted` (ascending by position), with that fingerprint — the
/// first to reach the count, in step order.
pub(crate) fn peak(sorted: &[Slot], window: usize) -> Option<(u64, u64)> {
    let window = window.max(1);
    let mut counts: HashMap<u64, u64> = HashMap::new();
    let mut best: Option<(u64, u64)> = None;
    for (index, slot) in sorted.iter().enumerate() {
        if index >= window {
            let leaving = sorted[index - window].fingerprint;
            if let Some(count) = counts.get_mut(&leaving) {
                *count -= 1;
                if *count == 0 {
                    counts.remove(&leaving);
                }
            }
        }
        let count = counts.entry(slot.fingerprint).or_insert(0);
        *count += 1;
        if best.map_or(true, |(_, top)| *count > top) {
            best = Some((slot.fingerprint, *count));
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slots(fingerprints: &[u64]) -> Vec<Slot> {
        fingerprints
            .iter()
            .enumerate()
            .map(|(index, fingerprint)| Slot {
                at: (index as u64, 1),
                fingerprint: *fingerprint,
            })
            .collect()
    }

    #[test]
    fn the_top_counts_only_the_window_and_breaks_ties_toward_the_latest() {
        let mut recent = RecentSteps::default();
        for slot in slots(&[7, 7, 7, 1, 2, 1, 2]) {
            assert!(recent.append(slot));
        }
        assert_eq!(recent.top(7), Some((7, 3)));
        // Over the last four, 1 and 2 occur twice each; 2 is the latest.
        assert_eq!(recent.top(4), Some((2, 2)));
        assert_eq!(recent.top(1), Some((2, 1)));
        assert_eq!(RecentSteps::default().top(5), None);
        // A slot that does not fall after the latest is not appended.
        assert!(!recent.append(Slot {
            at: (3, 9),
            fingerprint: 7
        }));
    }

    #[test]
    fn the_peak_is_the_densest_window_anywhere_in_the_run() {
        let run = slots(&[3, 3, 3, 1, 2, 1, 2, 4, 5, 6]);
        assert_eq!(peak(&run, 3), Some((3, 3)), "the early loop");
        assert_eq!(peak(&run, 2), Some((3, 2)), "the first to reach two");
        assert_eq!(peak(&[], 3), None);
        // The same fingerprint spread out never fills a narrow window.
        let spread = slots(&[9, 1, 2, 9, 3, 4, 9]);
        assert_eq!(peak(&spread, 3).map(|(_, count)| count), Some(1));
        assert_eq!(peak(&spread, 7), Some((9, 3)));
    }

    #[test]
    fn only_the_latest_steps_are_kept() {
        let mut recent = RecentSteps::default();
        let cap = MAX_REPEAT_WINDOW_STEPS as usize;
        let many: Vec<u64> = (0..cap as u64 + 10).collect();
        for slot in slots(&many) {
            recent.append(slot);
        }
        assert_eq!(recent.slots.len(), cap);
        assert_eq!(recent.slots.front().map(|slot| slot.fingerprint), Some(10));
        let mut rebuilt = RecentSteps::default();
        rebuilt.rebuild(&slots(&many));
        assert_eq!(rebuilt.slots, recent.slots);
    }
}
