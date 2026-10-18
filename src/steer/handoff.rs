//! Handoffs: control passing between NAMED agents within a run, and the
//! cycles — A → B → A — in which two of them pass it back and forth.
//!
//! # The definition, shared by the guard and the topology
//!
//! A run's **handoff sequence** is its steps (LLM, tool and agent spans)
//! that name an agent ([`crate::semconv::StepFacts::agent`]:
//! `gen_ai.agent.name`, Traceloop's entity name on an agent or workflow span,
//! `agent.name`), in step order — `(start, trace, span)` — with consecutive
//! steps of one agent merged into one **turn**. A step that names no agent
//! does not move control: it is work done inside whichever agent holds it.
//!
//! A **cycle** is control returning to an agent straight from the agent it
//! handed off to: turns `A, B, A`. Cycles are counted per ORDERED pair —
//! `(A, B)` counts the returns to A from B — so a supervisor that delegates
//! to one worker three times in a row (`S W S W S W S`) has three `(S, W)`
//! cycles and two `(W, S)`, and two agents bouncing a task between them
//! eight times have three or four each way. The guard's
//! `max_handoff_cycles` limit holds a run to the largest of these counts.
//!
//! # Incremental, exact, bounded
//!
//! [`HandoffTracker`] keeps the counts current as steps arrive in any order
//! and as re-sent spans move or rename a step, in O(log n) per change: a
//! change can only alter the turns next to it, so the counts are corrected
//! by recounting a window of at most nine turns around it, before and after.
//! The tests check it against [`cycles_in`] over the whole sequence, which
//! is what the topology computes from the store, so the guard and the
//! topology cannot disagree about a run they both see whole.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::Hash;
use std::ops::Bound::{self, Excluded, Included, Unbounded};

/// A step's place in step order: its start time, then a key for its
/// identity that breaks ties between steps starting together.
pub(crate) type Position = (u64, u64);

/// Longest agent name kept, in characters; a longer one is cut with `…`.
/// Names come from telemetry, and a reason line or a topology node is not
/// the place for an unbounded one.
pub const MAX_AGENT_NAME_CHARS: usize = 160;

/// Agent names one tracker keeps for rendering. Past it, an agent renders
/// as `agent#<hex>` — still distinct, never unbounded.
const MAX_NAMES: usize = 256;

/// `name`, cut to [`MAX_AGENT_NAME_CHARS`].
pub(crate) fn clip_name(name: &str) -> String {
    match name.char_indices().nth(MAX_AGENT_NAME_CHARS) {
        Some((end, _)) => format!("{}…", &name[..end]),
        None => name.to_owned(),
    }
}

/// A non-zero 64-bit key for an agent name.
pub(crate) fn agent_key(name: &str) -> u64 {
    let digest = crate::hash::hash128(name.as_bytes());
    let mut low = [0u8; 8];
    low.copy_from_slice(&digest.as_bytes()[..8]);
    u64::from_le_bytes(low).max(1)
}

/// `agents` with consecutive repeats merged: the turns.
#[cfg(test)]
pub(crate) fn turns_of<T: PartialEq>(agents: impl IntoIterator<Item = T>) -> Vec<T> {
    let mut turns: Vec<T> = Vec::new();
    for agent in agents {
        if turns.last() != Some(&agent) {
            turns.push(agent);
        }
    }
    turns
}

/// The cycles in `turns` (consecutive entries distinct, as [`turns_of`]
/// makes them): for every `i` with `turns[i] == turns[i + 2]`, one for the
/// ordered pair `(turns[i], turns[i + 1])`.
pub(crate) fn cycles_in<T: Eq + Hash + Clone>(turns: &[T]) -> HashMap<(T, T), u64> {
    let mut cycles: HashMap<(T, T), u64> = HashMap::new();
    for window in turns.windows(3) {
        if window[0] == window[2] && window[0] != window[1] {
            *cycles
                .entry((window[0].clone(), window[1].clone()))
                .or_default() += 1;
        }
    }
    cycles
}

/// The handoff sequence of one run, kept current incrementally; see the
/// module docs. Holds about a hundred bytes per named step.
#[derive(Debug, Default)]
pub(crate) struct HandoffTracker {
    /// Named steps by position: their agent.
    steps: BTreeMap<Position, u64>,
    /// Where each named step sits, by the caller's span key.
    at: HashMap<u64, Position>,
    /// The first step of each turn: its agent.
    turns: BTreeMap<Position, u64>,
    /// Cycles per ordered pair `(returned to, returned from)`.
    cycles: HashMap<(u64, u64), u64>,
    /// The same counts, ordered, so the largest is the last.
    ranked: BTreeSet<(u64, u64, u64)>,
    /// Agent names by key, at most [`MAX_NAMES`].
    names: HashMap<u64, String>,
}

impl HandoffTracker {
    /// Whether no named step is held.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Whether the step under `span` is held at `position` for `agent`, so
    /// that an upsert of it would change nothing.
    pub(crate) fn holds_as(&self, span: u64, position: Position, agent: u64) -> bool {
        self.at.get(&span) == Some(&position) && self.steps.get(&position) == Some(&agent)
    }

    /// Whether a step is held under `span`.
    pub(crate) fn holds(&self, span: u64) -> bool {
        self.at.contains_key(&span)
    }

    /// Places the step under `span` at `position`, naming agent `name`, in
    /// place of wherever it was. A no-op when nothing changes.
    pub(crate) fn upsert(&mut self, span: u64, position: Position, name: &str) {
        let agent = agent_key(name);
        if self.holds_as(span, position, agent) {
            return;
        }
        self.remove(span);
        if self.names.len() < MAX_NAMES || self.names.contains_key(&agent) {
            self.names.entry(agent).or_insert_with(|| clip_name(name));
        }
        // A position already taken by another span (a hash collision of two
        // span keys starting together) keeps the step it has: the tracker
        // never holds two steps at one place.
        if self.steps.contains_key(&position) {
            return;
        }
        let (low, high) = self.anchors(position);
        let before = self.window(low, high);
        let previous = self
            .steps
            .range(..position)
            .next_back()
            .map(|(at, agent)| (*at, *agent));
        let next = self
            .steps
            .range((Excluded(position), Unbounded))
            .next()
            .map(|(at, agent)| (*at, *agent));
        self.steps.insert(position, agent);
        self.at.insert(span, position);
        if previous.is_some_and(|(_, previous)| previous == agent) {
            // It joins the turn it falls in.
        } else if let Some((next_at, _)) = next.filter(|(_, next)| *next == agent) {
            // The next step is a turn of the same agent, which now starts
            // here.
            self.turns.remove(&next_at);
            self.turns.insert(position, agent);
        } else {
            self.turns.insert(position, agent);
            // Inside another agent's turn: the rest of it is a turn again.
            if let (Some((_, previous)), Some((next_at, next))) = (previous, next) {
                if previous == next {
                    self.turns.insert(next_at, next);
                }
            }
        }
        let after = self.window(low, high);
        self.recount(&before, &after);
    }

    /// Removes the step under `span`, if one is held.
    pub(crate) fn remove(&mut self, span: u64) {
        let Some(position) = self.at.get(&span).copied() else {
            return;
        };
        let (low, high) = self.anchors(position);
        let before = self.window(low, high);
        self.at.remove(&span);
        let Some(agent) = self.steps.remove(&position) else {
            return;
        };
        if self.turns.remove(&position).is_some() {
            let previous = self
                .steps
                .range(..position)
                .next_back()
                .map(|(_, agent)| *agent);
            let next = self
                .steps
                .range((Excluded(position), Unbounded))
                .next()
                .map(|(at, agent)| (*at, *agent));
            match next {
                // Its turn goes on from the next step.
                Some((next_at, next)) if next == agent => {
                    self.turns.insert(next_at, agent);
                }
                // It was a turn of its own: its neighbours may now be one.
                Some((next_at, next)) if previous == Some(next) => {
                    self.turns.remove(&next_at);
                }
                _ => {}
            }
        }
        let after = self.window(low, high);
        self.recount(&before, &after);
    }

    /// The largest cycle count, with its pair `(returned to, returned
    /// from)`; ties go to the larger keys, so the answer depends on the
    /// steps held and never on the order they arrived in.
    pub(crate) fn top(&self) -> Option<(u64, u64, u64)> {
        self.ranked
            .iter()
            .next_back()
            .map(|(count, to, from)| (*count, *to, *from))
    }

    /// The name of agent `key`, or `agent#<hex>` past the name bound.
    pub(crate) fn name(&self, key: u64) -> String {
        self.names
            .get(&key)
            .cloned()
            .unwrap_or_else(|| format!("agent#{key:016x}"))
    }

    /// The window a change at `position` is recounted over: from the fourth
    /// turn starting at or before it to the third starting after it. A
    /// change alters at most the turn it falls in and its two neighbours, so
    /// every cycle it can make or break lies inside, and the bounds
    /// themselves are turns it cannot move.
    fn anchors(&self, position: Position) -> (Bound<Position>, Bound<Position>) {
        let low = self
            .turns
            .range(..=position)
            .rev()
            .nth(3)
            .map_or(Unbounded, |(at, _)| Included(*at));
        let high = self
            .turns
            .range((Excluded(position), Unbounded))
            .nth(2)
            .map_or(Unbounded, |(at, _)| Included(*at));
        (low, high)
    }

    fn window(&self, low: Bound<Position>, high: Bound<Position>) -> Vec<u64> {
        self.turns
            .range((low, high))
            .map(|(_, agent)| *agent)
            .collect()
    }

    /// Moves the counts from what `before` held to what `after` holds.
    fn recount(&mut self, before: &[u64], after: &[u64]) {
        let mut delta: HashMap<(u64, u64), i64> = HashMap::new();
        for (pair, count) in cycles_in(after) {
            *delta.entry(pair).or_default() += count as i64;
        }
        for (pair, count) in cycles_in(before) {
            *delta.entry(pair).or_default() -= count as i64;
        }
        for (pair, change) in delta {
            if change == 0 {
                continue;
            }
            let held = self.cycles.get(&pair).copied().unwrap_or(0);
            let next = (held as i64 + change).max(0) as u64;
            if held > 0 {
                self.ranked.remove(&(held, pair.0, pair.1));
            }
            if next == 0 {
                self.cycles.remove(&pair);
            } else {
                self.cycles.insert(pair, next);
                self.ranked.insert((next, pair.0, pair.1));
            }
        }
    }

    /// Every cycle count, for tests.
    #[cfg(test)]
    fn counts(&self) -> HashMap<(u64, u64), u64> {
        self.cycles.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turns(names: &[&str]) -> Vec<String> {
        turns_of(names.iter().map(|name| (*name).to_owned()))
    }

    #[test]
    fn cycles_count_returns_per_ordered_pair() {
        let supervisor = turns(&["S", "W", "S", "W", "S", "W", "S"]);
        let cycles = cycles_in(&supervisor);
        assert_eq!(cycles[&("S".to_owned(), "W".to_owned())], 3);
        assert_eq!(cycles[&("W".to_owned(), "S".to_owned())], 2);
        // A supervisor fanning out to three workers returns from each once,
        // and no worker ever gets control straight back.
        let fan_out = turns(&["S", "A", "S", "B", "S", "C", "S"]);
        let cycles = cycles_in(&fan_out);
        assert_eq!(cycles.len(), 3, "{cycles:?}");
        assert!(cycles
            .iter()
            .all(|((to, _), count)| to == "S" && *count == 1));
        assert!(cycles_in(&turns(&["A", "B", "C", "D"])).is_empty());
        assert!(cycles_in(&turns(&["A"])).is_empty());
    }

    #[test]
    fn consecutive_steps_of_one_agent_are_one_turn() {
        assert_eq!(
            turns(&["A", "A", "B", "B", "B", "A", "A"]),
            vec!["A".to_owned(), "B".to_owned(), "A".to_owned()]
        );
        let cycles = cycles_in(&turns(&["A", "A", "B", "B", "A"]));
        assert_eq!(cycles[&("A".to_owned(), "B".to_owned())], 1);
    }

    /// The whole sequence recounted from scratch, as the topology does.
    fn brute(tracker: &HandoffTracker) -> HashMap<(u64, u64), u64> {
        cycles_in(&turns_of(tracker.steps.values().copied()))
    }

    #[test]
    fn steps_arriving_out_of_order_count_as_the_sequence_does() {
        let mut tracker = HandoffTracker::default();
        // S W S W S, delivered last-first.
        for (index, agent) in ["S", "W", "S", "W", "S"].iter().enumerate().rev() {
            tracker.upsert(index as u64, (index as u64 * 10, index as u64), agent);
        }
        let (count, to, from) = tracker.top().expect("cycles");
        assert_eq!(count, 2);
        assert_eq!(
            (tracker.name(to), tracker.name(from)),
            ("S".into(), "W".into())
        );
        assert_eq!(tracker.counts(), brute(&tracker));
        // A step landing inside W's second turn splits nothing (same agent);
        // one of another agent splits it.
        tracker.upsert(100, (31, 100), "W");
        assert_eq!(tracker.counts(), brute(&tracker));
        tracker.upsert(101, (32, 101), "X");
        assert_eq!(tracker.counts(), brute(&tracker));
        // Moving a step and renaming it are replacements.
        tracker.upsert(101, (5, 101), "X");
        assert_eq!(tracker.counts(), brute(&tracker));
        tracker.upsert(101, (5, 101), "S");
        assert_eq!(tracker.counts(), brute(&tracker));
        for span in [0, 1, 2, 3, 4, 100, 101] {
            tracker.remove(span);
            assert_eq!(tracker.counts(), brute(&tracker));
        }
        assert!(tracker.is_empty() && tracker.top().is_none());
        assert!(tracker.turns.is_empty() && tracker.ranked.is_empty());
    }

    #[test]
    fn random_changes_match_a_full_recount() {
        let mut rng = crate::seed::Rng::new(0x5eed_ca11);
        for round in 0..200 {
            let mut tracker = HandoffTracker::default();
            let agents = ["A", "B", "C"];
            let spans = 3 + (round % 20) as u64;
            for _ in 0..(spans * 4) {
                let span = rng.range(0, spans - 1);
                if rng.chance(25) {
                    tracker.remove(span);
                } else {
                    // Few distinct starts, so ties break on the span key.
                    let start = rng.range(0, spans * 2);
                    let agent = *rng.pick(&agents);
                    tracker.upsert(span, (start, span), agent);
                }
                assert_eq!(tracker.counts(), brute(&tracker), "round {round}");
                let expected_top = brute(&tracker)
                    .into_iter()
                    .map(|((to, from), count)| (count, to, from))
                    .max();
                assert_eq!(tracker.top(), expected_top, "round {round}");
                // Turns are exactly the agent changes of the steps.
                let starts: Vec<u64> = tracker.turns.values().copied().collect();
                assert_eq!(starts, turns_of(tracker.steps.values().copied()));
            }
        }
    }

    #[test]
    fn names_are_clipped_and_bounded() {
        let long = "x".repeat(400);
        let mut tracker = HandoffTracker::default();
        tracker.upsert(1, (1, 1), &long);
        let key = agent_key(&long);
        assert_eq!(tracker.name(key).chars().count(), MAX_AGENT_NAME_CHARS + 1);
        for index in 0..(MAX_NAMES as u64 + 10) {
            tracker.upsert(
                10 + index,
                (10 + index, 10 + index),
                &format!("agent-{index}"),
            );
        }
        assert!(tracker.names.len() <= MAX_NAMES);
        let unnamed = agent_key(&format!("agent-{}", MAX_NAMES + 5));
        assert!(tracker.name(unnamed).starts_with("agent#"));
    }
}
