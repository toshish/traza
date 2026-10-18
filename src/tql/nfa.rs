//! TQL patterns compiled to an NFA, and the matcher that runs it over a
//! run's steps.
//!
//! **Semantics: leftmost-first**, the rule Perl, PCRE and RE2 share. Of all
//! the ways a pattern can match, the one reported starts at the EARLIEST
//! step; among those, it is the one a backtracking matcher would find first
//! — alternatives tried left to right, quantifiers greedy (more repetitions
//! first) unless marked lazy with `?`, and the steps a `..>` gap skips as
//! FEW as possible. After a match, the search resumes at the step after it,
//! so occurrences never overlap.
//!
//! **The matcher is a Pike VM**: every live path through the program
//! advances in lock step, one step of the run at a time, in priority order.
//! Two paths that reach the same instruction with the same captures (and,
//! under `within`, the same start) have identical futures, so only the
//! higher-priority one is kept — which is what makes the cost
//! `steps × live paths` rather than exponential, and why the answer equals
//! the backtracking one exactly (the property tests below check that
//! against a literal backtracking reference on random patterns and runs).
//! Paths are capped at [`MAX_THREADS`] per step, the environments one
//! evaluation may fork at what the next step's list still has room for, and
//! the states a list remembers at [`MAX_SEEN`]; past any cap the
//! lowest-priority alternatives are dropped and the result says `inexact`.
//! All the work — paths advanced, states reached, environments forked,
//! regex instructions stepped — is charged to one [`Meter`], which checks
//! the query deadline in proportion to it.

use std::collections::HashSet;
use std::rc::Rc;

use super::ast::{Atom, Gap, Node, Pred};
use super::meter::{Meter, PATH_UNITS, STATE_UNITS};
use super::step::{atom_uses_captures, eval_atom, pred_uses_captures, test_pred, Env, Step};
use super::Within;
use crate::{Deadline, Result};

/// Largest compiled program. Counted repetition expands the program, so
/// this bounds memory and per-step cost alike.
pub(crate) const MAX_PROGRAM: usize = 20_000;

/// Live paths kept per step of the run.
pub(crate) const MAX_THREADS: usize = 4096;

/// States (instruction, captures, start) one list remembers at most when
/// captures or `within` key them. Each live path passes through a handful of
/// states between two steps, so this leaves room for every path the list can
/// hold while bounding what a flood of distinct captures can allocate.
pub(crate) const MAX_SEEN: usize = 16 * MAX_THREADS;

/// Bytes of an environment hashed per unit of work.
const HASH_BYTES_PER_UNIT: u64 = 64;

#[derive(Clone, Debug)]
pub(crate) enum Inst {
    /// Consume one step satisfying `tests[test]`; `record` says whether the
    /// step is part of the match's reported spans (gap steps are not).
    Consume {
        test: usize,
        record: bool,
    },
    /// Try `first`, then `second`.
    Split(usize, usize),
    Jmp(usize),
    AssertStart,
    AssertEnd,
    Match,
}

pub(crate) enum TestKind {
    Atom(Atom),
    Gap(Option<Pred>),
}

pub(crate) struct CompiledTest {
    kind: TestKind,
    /// Reads no capture, so one evaluation per step serves every path.
    pure: bool,
}

/// A compiled pattern.
pub(crate) struct Program {
    pub(crate) insts: Vec<Inst>,
    tests: Vec<CompiledTest>,
    pub(crate) slots: usize,
    pub(crate) within: Option<Within>,
}

/// Compiles a parsed pattern.
pub(crate) fn compile(
    node: &Node,
    slots: usize,
    within: Option<Within>,
) -> std::result::Result<Program, String> {
    let mut compiler = Compiler {
        insts: Vec::new(),
        tests: Vec::new(),
        atom_ids: Vec::new(),
    };
    compiler.emit(node)?;
    compiler.push(Inst::Match)?;
    Ok(Program {
        insts: compiler.insts,
        tests: compiler.tests,
        slots,
        within,
    })
}

struct Compiler {
    insts: Vec<Inst>,
    tests: Vec<CompiledTest>,
    /// The test id of each AST node already compiled, by address, so the
    /// copies a counted repetition emits share one test (and its memo).
    atom_ids: Vec<(usize, usize)>,
}

impl Compiler {
    fn push(&mut self, inst: Inst) -> std::result::Result<usize, String> {
        if self.insts.len() >= MAX_PROGRAM {
            return Err(format!(
                "the pattern expands past {MAX_PROGRAM} instructions; lower its repetition counts"
            ));
        }
        self.insts.push(inst);
        Ok(self.insts.len() - 1)
    }

    fn test_id(&mut self, address: usize, make: impl FnOnce() -> CompiledTest) -> usize {
        if let Some((_, id)) = self.atom_ids.iter().find(|(known, _)| *known == address) {
            return *id;
        }
        self.tests.push(make());
        let id = self.tests.len() - 1;
        self.atom_ids.push((address, id));
        id
    }

    fn emit(&mut self, node: &Node) -> std::result::Result<(), String> {
        match node {
            Node::Atom(atom) => {
                let address = &**atom as *const Atom as usize;
                let test = self.test_id(address, || CompiledTest {
                    pure: !atom_uses_captures(atom),
                    kind: TestKind::Atom((**atom).clone()),
                });
                self.push(Inst::Consume { test, record: true })?;
            }
            Node::Gap(gap) => {
                let address = &**gap as *const Gap as usize;
                let test = self.test_id(address, || CompiledTest {
                    pure: !gap.pred.as_ref().is_some_and(pred_uses_captures),
                    kind: TestKind::Gap(gap.pred.clone()),
                });
                self.repeat(gap.min, gap.max, false, &mut |compiler| {
                    compiler
                        .push(Inst::Consume {
                            test,
                            record: false,
                        })
                        .map(|_| ())
                })?;
            }
            Node::Concat(items) => {
                for item in items {
                    self.emit(item)?;
                }
            }
            Node::Alternate(branches) => {
                let mut jumps = Vec::new();
                for (index, branch) in branches.iter().enumerate() {
                    if index + 1 < branches.len() {
                        let split = self.push(Inst::Split(0, 0))?;
                        self.emit(branch)?;
                        jumps.push(self.push(Inst::Jmp(0))?);
                        let next = self.insts.len();
                        self.insts[split] = Inst::Split(split + 1, next);
                    } else {
                        self.emit(branch)?;
                    }
                }
                let end = self.insts.len();
                for jump in jumps {
                    self.insts[jump] = Inst::Jmp(end);
                }
            }
            Node::Repeat {
                node,
                min,
                max,
                greedy,
            } => self.repeat(*min, *max, *greedy, &mut |compiler| compiler.emit(node))?,
            Node::Start => {
                self.push(Inst::AssertStart)?;
            }
            Node::End => {
                self.push(Inst::AssertEnd)?;
            }
        }
        Ok(())
    }

    fn repeat(
        &mut self,
        min: u32,
        max: Option<u32>,
        greedy: bool,
        body: &mut dyn FnMut(&mut Self) -> std::result::Result<(), String>,
    ) -> std::result::Result<(), String> {
        for _ in 0..min {
            body(self)?;
        }
        let split = |at: usize, end: usize| {
            if greedy {
                Inst::Split(at + 1, end)
            } else {
                Inst::Split(end, at + 1)
            }
        };
        match max {
            None => {
                let at = self.push(Inst::Split(0, 0))?;
                body(self)?;
                self.push(Inst::Jmp(at))?;
                let end = self.insts.len();
                self.insts[at] = split(at, end);
            }
            Some(max) => {
                let mut splits = Vec::new();
                for _ in min..max {
                    splits.push(self.push(Inst::Split(0, 0))?);
                    body(self)?;
                }
                let end = self.insts.len();
                for at in splits {
                    self.insts[at] = split(at, end);
                }
            }
        }
        Ok(())
    }
}

// ----------------------------------------------------------------- matcher

/// The steps a path consumed as part of the match, newest first.
struct PathNode {
    step: usize,
    previous: Option<Rc<PathNode>>,
}

impl Drop for PathNode {
    // Iterative, so dropping a long path cannot overflow the stack.
    fn drop(&mut self) {
        let mut next = self.previous.take();
        while let Some(node) = next {
            match Rc::try_unwrap(node) {
                Ok(mut node) => next = node.previous.take(),
                Err(_) => break,
            }
        }
    }
}

fn collect_path(path: &Option<Rc<PathNode>>) -> Vec<usize> {
    let mut out = Vec::new();
    let mut current = path.as_ref();
    while let Some(node) = current {
        out.push(node.step);
        current = node.previous.as_ref();
    }
    out.reverse();
    out
}

#[derive(Clone)]
struct Thread {
    pc: usize,
    env: Env,
    start: usize,
    path: Option<Rc<PathNode>>,
}

/// Which (instruction, captures, start) states a list already holds.
enum Seen {
    /// No captures and no `within`: the instruction alone is the state.
    Dense {
        stamp: Vec<u32>,
        generation: u32,
    },
    Hashed(HashSet<(usize, usize, Env)>),
}

impl Seen {
    fn new(program: &Program) -> Self {
        if program.slots == 0 && program.within.is_none() {
            Seen::Dense {
                stamp: vec![0; program.insts.len()],
                generation: 1,
            }
        } else {
            Seen::Hashed(HashSet::new())
        }
    }

    fn clear(&mut self) {
        match self {
            Seen::Dense { stamp, generation } => {
                *generation = generation.wrapping_add(1);
                if *generation == 0 {
                    stamp.iter_mut().for_each(|slot| *slot = 0);
                    *generation = 1;
                }
            }
            Seen::Hashed(set) => set.clear(),
        }
    }

    fn contains(&self, pc: usize, start: usize, env: &Env) -> bool {
        match self {
            Seen::Dense { stamp, generation } => stamp[pc] == *generation,
            Seen::Hashed(set) => set.contains(&(pc, start, env.clone())),
        }
    }

    fn insert(&mut self, pc: usize, start: usize, env: &Env) -> Inserted {
        match self {
            Seen::Dense { stamp, generation } => {
                if stamp[pc] == *generation {
                    return Inserted::Known;
                }
                stamp[pc] = *generation;
                Inserted::New
            }
            Seen::Hashed(set) => {
                let key = (pc, start, env.clone());
                if set.contains(&key) {
                    Inserted::Known
                } else if set.len() >= MAX_SEEN {
                    Inserted::Full
                } else {
                    set.insert(key);
                    Inserted::New
                }
            }
        }
    }
}

/// What remembering a state did.
#[derive(Debug, PartialEq, Eq)]
enum Inserted {
    New,
    /// Already held: a higher-priority path got there first.
    Known,
    /// Not held, and no room to remember it ([`MAX_SEEN`]).
    Full,
}

struct List {
    threads: Vec<Thread>,
    seen: Seen,
}

/// One match.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Found {
    /// First step the match covers.
    pub(crate) start: usize,
    /// Last step it covers (inclusive).
    pub(crate) end: usize,
    /// The steps its atoms consumed, in order (gap steps excluded).
    pub(crate) path: Vec<usize>,
    /// The captures at the end of the match.
    pub(crate) env: Env,
}

/// Every non-overlapping match in a run, as far as the caps allow.
#[derive(Clone, Debug, Default)]
pub(crate) struct Scan {
    /// The first matches, at most the `keep` asked for.
    pub(crate) found: Vec<Found>,
    /// Matches counted, at most the `max_count` asked for.
    pub(crate) count: usize,
    /// Whether counting stopped at `max_count`.
    pub(crate) count_capped: bool,
    /// Whether a path cap dropped paths, so a match may have been missed or
    /// reported later than the leftmost-first one.
    pub(crate) inexact: bool,
}

impl Program {
    fn within_holds(&self, steps: &[Step<'_>], start: usize, at: usize) -> bool {
        match self.within {
            None => true,
            Some(Within::Steps(limit)) => (at - start + 1) as u64 <= limit,
            Some(Within::Duration(limit)) => {
                steps[at].start_ns().saturating_sub(steps[start].start_ns()) <= limit
            }
        }
    }

    /// Adds `thread` and every state reachable from it without consuming a
    /// step, in priority order. A state the list has no room for — its paths
    /// are full, or it cannot remember another state — is dropped, and the
    /// meter says so; nothing is remembered for it, so a flood of distinct
    /// captures allocates no more than the caps.
    fn add(
        &self,
        list: &mut List,
        thread: Thread,
        position: usize,
        steps: usize,
        meter: &mut Meter,
    ) -> Result<()> {
        let keyed_start = self.within.is_some();
        // Every state of one closure shares the thread's captures: what
        // remembering one costs is fixed for the call.
        let units = match list.seen {
            Seen::Dense { .. } => STATE_UNITS,
            Seen::Hashed(_) => STATE_UNITS + thread.env.weight() / HASH_BYTES_PER_UNIT,
        };
        let mut stack = vec![thread];
        while let Some(thread) = stack.pop() {
            meter.charge(units)?;
            let start = if keyed_start { thread.start } else { 0 };
            if list.threads.len() >= MAX_THREADS {
                if !list.seen.contains(thread.pc, start, &thread.env) {
                    meter.dropped = true;
                }
                continue;
            }
            match list.seen.insert(thread.pc, start, &thread.env) {
                Inserted::New => {}
                Inserted::Known => continue,
                Inserted::Full => {
                    meter.dropped = true;
                    continue;
                }
            }
            match self.insts[thread.pc] {
                Inst::Jmp(target) => stack.push(Thread {
                    pc: target,
                    ..thread
                }),
                Inst::Split(first, second) => {
                    stack.push(Thread {
                        pc: second,
                        ..thread.clone()
                    });
                    stack.push(Thread {
                        pc: first,
                        ..thread
                    });
                }
                Inst::AssertStart => {
                    if position == 0 {
                        stack.push(Thread {
                            pc: thread.pc + 1,
                            ..thread
                        });
                    }
                }
                Inst::AssertEnd => {
                    if position == steps {
                        stack.push(Thread {
                            pc: thread.pc + 1,
                            ..thread
                        });
                    }
                }
                Inst::Consume { .. } | Inst::Match => list.threads.push(thread),
            }
        }
        Ok(())
    }

    /// Evaluates test `id` on `step` for a path holding `env`, pushing at
    /// most `limit` environments onto `out`.
    #[allow(clippy::too_many_arguments)]
    fn evaluate(
        &self,
        id: usize,
        step: &Step<'_>,
        env: &Env,
        memo: &mut [Option<bool>],
        out: &mut Vec<Env>,
        limit: usize,
        meter: &mut Meter,
    ) -> Result<()> {
        let test = &self.tests[id];
        if test.pure {
            let holds = match memo[id] {
                Some(holds) => holds,
                None => {
                    let holds = match &test.kind {
                        TestKind::Atom(atom) => {
                            // A pure atom forks nothing: one environment at most.
                            let mut scratch = Vec::new();
                            eval_atom(atom, step, env, &mut scratch, 1, meter)?;
                            !scratch.is_empty()
                        }
                        TestKind::Gap(pred) => match pred {
                            Some(pred) => test_pred(pred, step, env, meter)?,
                            None => true,
                        },
                    };
                    memo[id] = Some(holds);
                    holds
                }
            };
            if holds {
                if out.len() < limit {
                    out.push(env.clone());
                } else {
                    meter.dropped = true;
                }
            }
            return Ok(());
        }
        match &test.kind {
            TestKind::Atom(atom) => eval_atom(atom, step, env, out, limit, meter)?,
            TestKind::Gap(pred) => {
                let holds = match pred {
                    Some(pred) => test_pred(pred, step, env, meter)?,
                    None => true,
                };
                if holds {
                    if out.len() < limit {
                        out.push(env.clone());
                    } else {
                        meter.dropped = true;
                    }
                }
            }
        }
        Ok(())
    }

    /// The leftmost-first match starting at or after `from`.
    fn find(&self, steps: &[Step<'_>], from: usize, meter: &mut Meter) -> Result<Option<Found>> {
        let n = steps.len();
        let mut current = List {
            threads: Vec::new(),
            seen: Seen::new(self),
        };
        let mut next = List {
            threads: Vec::new(),
            seen: Seen::new(self),
        };
        let mut memo = vec![None; self.tests.len()];
        let mut envs = Vec::new();
        let mut best: Option<Found> = None;
        let mut position = from;
        loop {
            if best.is_none() && position < n {
                let seed = Thread {
                    pc: 0,
                    env: Env::new(self.slots),
                    start: position,
                    path: None,
                };
                self.add(&mut current, seed, position, n, meter)?;
            }
            memo.iter_mut().for_each(|slot| *slot = None);
            for thread in std::mem::take(&mut current.threads) {
                meter.charge(PATH_UNITS)?;
                match self.insts[thread.pc] {
                    Inst::Match => {
                        // Every lower-priority path is cut; higher-priority
                        // ones already advanced and may still win.
                        best = Some(Found {
                            start: thread.start,
                            end: position - 1,
                            path: collect_path(&thread.path),
                            env: thread.env,
                        });
                        break;
                    }
                    Inst::Consume { test, record } => {
                        if position >= n || !self.within_holds(steps, thread.start, position) {
                            continue;
                        }
                        envs.clear();
                        // An evaluation may fork no more paths than the next
                        // step's list still has room for.
                        let room = MAX_THREADS.saturating_sub(next.threads.len());
                        self.evaluate(
                            test,
                            &steps[position],
                            &thread.env,
                            &mut memo,
                            &mut envs,
                            room,
                            meter,
                        )?;
                        for env in envs.drain(..) {
                            let path = if record {
                                Some(Rc::new(PathNode {
                                    step: position,
                                    previous: thread.path.clone(),
                                }))
                            } else {
                                thread.path.clone()
                            };
                            let advanced = Thread {
                                pc: thread.pc + 1,
                                env,
                                start: thread.start,
                                path,
                            };
                            self.add(&mut next, advanced, position + 1, n, meter)?;
                        }
                    }
                    _ => {}
                }
            }
            if position >= n {
                break;
            }
            std::mem::swap(&mut current, &mut next);
            next.threads.clear();
            next.seen.clear();
            position += 1;
            if current.threads.is_empty() && best.is_some() {
                break;
            }
        }
        Ok(best)
    }

    /// Every non-overlapping match, keeping the first `keep` and counting up
    /// to `max_count`. All the work is charged against `deadline`.
    pub(crate) fn scan(
        &self,
        steps: &[Step<'_>],
        keep: usize,
        max_count: usize,
        deadline: Option<Deadline>,
    ) -> Result<Scan> {
        let mut meter = Meter::new(deadline);
        let mut scan = Scan::default();
        let mut from = 0;
        while from < steps.len() {
            let Some(found) = self.find(steps, from, &mut meter)? else {
                break;
            };
            meter.charge(PATH_UNITS)?;
            scan.count += 1;
            from = found.end + 1;
            if scan.found.len() < keep {
                scan.found.push(found);
            }
            if scan.count >= max_count {
                scan.count_capped = from < steps.len();
                break;
            }
        }
        scan.inexact = meter.dropped;
        Ok(scan)
    }
}
#[cfg(test)]
pub(crate) mod tests {
    use super::super::ast::{Atom, Node};
    use super::super::step::tests::{step_span, steps};
    use super::super::step::{eval_atom, test_pred, Env, Step};
    use super::super::{Query, Within};
    use super::*;
    use crate::Span;
    use serde_json::json;

    /// The backtracking reference: walks the syntax tree directly, trying
    /// alternatives in priority order, and returns the first complete match
    /// — leftmost-first by construction.
    pub(crate) fn reference_find(query: &Query, steps: &[Step<'_>], from: usize) -> Option<Found> {
        type K<'k> = &'k mut dyn FnMut(usize, &Env, &mut Vec<usize>) -> Option<Found>;
        fn bt(
            node: &Node,
            steps: &[Step<'_>],
            at: usize,
            env: &Env,
            path: &mut Vec<usize>,
            k: K<'_>,
        ) -> Option<Found> {
            match node {
                Node::Atom(atom) => atom_step(atom, steps, at, env, path, k),
                Node::Gap(gap) => gap_steps(gap, 0, steps, at, env, path, k),
                Node::Concat(items) => seq(items, steps, at, env, path, k),
                Node::Alternate(branches) => {
                    for branch in branches {
                        if let Some(found) = bt(branch, steps, at, env, path, &mut *k) {
                            return Some(found);
                        }
                    }
                    None
                }
                Node::Repeat {
                    node,
                    min,
                    max,
                    greedy,
                } => repeat(node, *min, *max, *greedy, 0, steps, at, env, path, k),
                Node::Start => {
                    if at == 0 {
                        k(at, env, path)
                    } else {
                        None
                    }
                }
                Node::End => {
                    if at == steps.len() {
                        k(at, env, path)
                    } else {
                        None
                    }
                }
            }
        }
        fn atom_step(
            atom: &Atom,
            steps: &[Step<'_>],
            at: usize,
            env: &Env,
            path: &mut Vec<usize>,
            k: K<'_>,
        ) -> Option<Found> {
            if at >= steps.len() {
                return None;
            }
            let mut envs = Vec::new();
            eval_atom(
                atom,
                &steps[at],
                env,
                &mut envs,
                usize::MAX,
                &mut Meter::new(None),
            )
            .expect("no deadline");
            for next in envs {
                path.push(at);
                if let Some(found) = k(at + 1, &next, path) {
                    return Some(found);
                }
                path.pop();
            }
            None
        }
        #[allow(clippy::too_many_arguments)]
        fn gap_steps(
            gap: &super::super::ast::Gap,
            count: u32,
            steps: &[Step<'_>],
            at: usize,
            env: &Env,
            path: &mut Vec<usize>,
            k: K<'_>,
        ) -> Option<Found> {
            // Lazy: stop as early as allowed, then take one more.
            if count >= gap.min {
                if let Some(found) = k(at, env, path) {
                    return Some(found);
                }
            }
            if gap.max.is_some_and(|max| count >= max) || at >= steps.len() {
                return None;
            }
            let passes = gap.pred.as_ref().map_or(true, |pred| {
                test_pred(pred, &steps[at], env, &mut Meter::new(None)).expect("no deadline")
            });
            if !passes {
                return None;
            }
            gap_steps(gap, count + 1, steps, at + 1, env, path, k)
        }
        fn seq(
            items: &[Node],
            steps: &[Step<'_>],
            at: usize,
            env: &Env,
            path: &mut Vec<usize>,
            k: K<'_>,
        ) -> Option<Found> {
            match items.split_first() {
                None => k(at, env, path),
                Some((first, rest)) => bt(first, steps, at, env, path, &mut |next, env, path| {
                    seq(rest, steps, next, env, path, &mut *k)
                }),
            }
        }
        #[allow(clippy::too_many_arguments)]
        fn repeat(
            node: &Node,
            min: u32,
            max: Option<u32>,
            greedy: bool,
            count: u32,
            steps: &[Step<'_>],
            at: usize,
            env: &Env,
            path: &mut Vec<usize>,
            k: K<'_>,
        ) -> Option<Found> {
            let more_allowed = max.map_or(true, |max| count < max);
            let more = |path: &mut Vec<usize>, k: K<'_>| -> Option<Found> {
                if !more_allowed {
                    return None;
                }
                bt(node, steps, at, env, path, &mut |next, env, path| {
                    repeat(
                        node,
                        min,
                        max,
                        greedy,
                        count + 1,
                        steps,
                        next,
                        env,
                        path,
                        &mut *k,
                    )
                })
            };
            if greedy {
                if let Some(found) = more(path, &mut *k) {
                    return Some(found);
                }
                if count >= min {
                    return k(at, env, path);
                }
                None
            } else {
                if count >= min {
                    if let Some(found) = k(at, env, path) {
                        return Some(found);
                    }
                }
                more(path, k)
            }
        }
        let within = query.program.within;
        for start in from..steps.len() {
            let mut path = Vec::new();
            let found = bt(
                &query.node,
                steps,
                start,
                &Env::new(query.program.slots),
                &mut path,
                &mut |at, env, path| {
                    if at == start {
                        return None;
                    }
                    let end = at - 1;
                    let fits = match within {
                        None => true,
                        Some(Within::Steps(limit)) => (end - start + 1) as u64 <= limit,
                        Some(Within::Duration(limit)) => {
                            steps[end]
                                .start_ns()
                                .saturating_sub(steps[start].start_ns())
                                <= limit
                        }
                    };
                    fits.then(|| Found {
                        start,
                        end,
                        path: path.clone(),
                        env: env.clone(),
                    })
                },
            );
            if found.is_some() {
                return found;
            }
        }
        None
    }

    pub(crate) fn reference_scan(query: &Query, steps: &[Step<'_>]) -> Vec<Found> {
        let mut out = Vec::new();
        let mut from = 0;
        while from < steps.len() {
            match reference_find(query, steps, from) {
                Some(found) => {
                    from = found.end + 1;
                    out.push(found);
                }
                None => break,
            }
        }
        out
    }

    fn query(text: &str) -> Query {
        Query::compile(text).unwrap_or_else(|error| panic!("{text}: {error}"))
    }

    /// Steps from a compact spec: `T:a` tool a ok, `T:a!` tool a error,
    /// `L` an llm call, `A:x` agent x; `@f` suffix gives a tool a file.
    fn spans(spec: &str) -> Vec<Span> {
        spec.split_whitespace()
            .enumerate()
            .map(|(index, token)| {
                let (body, file) = token.split_once('@').map_or((token, None), |(b, f)| (b, Some(f)));
                let failed = body.ends_with('!');
                let body = body.trim_end_matches('!');
                let status = if failed { "error" } else { "ok" };
                let extra = match file {
                    Some(file) => json!({"gen_ai.tool.call.arguments": {"file_path": file, "new_string": "x"}}),
                    None => json!({}),
                };
                match body.split_once(':') {
                    Some(("T", name)) => step_span(index, "tool", name, status, extra),
                    Some(("A", name)) => step_span(index, "agent", name, status, extra),
                    _ => step_span(index, "llm", "m", status, extra),
                }
            })
            .collect()
    }

    fn matched(text: &str, spec: &str) -> Vec<(usize, usize, Vec<usize>)> {
        let spans = spans(spec);
        let steps = steps(&spans);
        let query = query(text);
        let scan = query
            .program
            .scan(&steps, usize::MAX, usize::MAX, None)
            .expect("scans");
        let reference = reference_scan(&query, &steps);
        assert_eq!(scan.found, reference, "{text} on {spec}");
        scan.found
            .into_iter()
            .map(|found| (found.start, found.end, found.path))
            .collect()
    }

    #[test]
    fn adjacency_eventually_and_alternation() {
        assert_eq!(
            matched("tool(a) -> tool(b)", "T:a T:b T:a L T:b"),
            [(0, 1, vec![0, 1])]
        );
        // `..>` skips as few steps as possible, and skipped steps are not
        // part of the reported path.
        assert_eq!(
            matched("tool(a) ..> tool(b)", "T:a L T:b T:a T:b"),
            [(0, 2, vec![0, 2]), (3, 4, vec![3, 4])]
        );
        assert_eq!(
            matched("tool(b) | llm", "T:a L T:b"),
            [(1, 1, vec![1]), (2, 2, vec![2])]
        );
    }

    #[test]
    fn greedy_and_lazy_quantifiers() {
        assert_eq!(
            matched("tool(a)+", "T:a T:a T:a L"),
            [(0, 2, vec![0, 1, 2])]
        );
        assert_eq!(
            matched("tool(a)+?", "T:a T:a L"),
            [(0, 0, vec![0]), (1, 1, vec![1])]
        );
        assert_eq!(
            matched("tool(a){2}", "T:a T:a T:a T:a T:a"),
            [(0, 1, vec![0, 1]), (2, 3, vec![2, 3])]
        );
        assert_eq!(
            matched("tool(a){2,3} -> llm", "T:a T:a T:a T:a L"),
            [(1, 4, vec![1, 2, 3, 4])]
        );
        assert_eq!(
            matched("tool(a) -> tool(b)? -> llm", "T:a L T:a T:b L"),
            [(0, 1, vec![0, 1]), (2, 4, vec![2, 3, 4])]
        );
    }

    #[test]
    fn anchors_are_absolute() {
        assert_eq!(matched("^llm", "L L"), [(0, 0, vec![0])]);
        assert_eq!(matched("llm$", "L T:a L"), [(2, 2, vec![2])]);
        assert!(matched("^tool(a)", "L T:a").is_empty());
    }

    #[test]
    fn captures_require_equal_values() {
        assert_eq!(
            matched("edit[file=@f] ..> edit[file=@f]", "T:e@x T:e@y L T:e@x"),
            [(0, 3, vec![0, 3])]
        );
        assert!(matched("edit[file=@f] -> edit[file=@f]", "T:e@x T:e@y").is_empty());
        assert_eq!(
            matched("tool(@t)[error] ..> tool(@t)[not error]", "T:a! T:b T:a"),
            [(0, 2, vec![0, 2])]
        );
    }

    #[test]
    fn within_bounds_the_whole_match() {
        assert!(matched("tool(a) ..> tool(b) within 3", "T:a L L T:b").is_empty());
        assert_eq!(
            matched("tool(a) ..> tool(b) within 4", "T:a L L T:b"),
            [(0, 3, vec![0, 3])]
        );
        // A later start can still fit when an earlier one cannot.
        assert_eq!(
            matched("tool(a) ..> tool(b) within 2", "T:a L T:a T:b"),
            [(2, 3, vec![2, 3])]
        );
        // Paths from different starts meet at the same point of the pattern:
        // the earlier one runs out of window, so the later one must not have
        // been merged into it.
        assert_eq!(
            matched("step ..> tool(b) within 3", "L L L L T:b"),
            [(2, 4, vec![2, 4])]
        );
        // Steps are a second apart in the fixture.
        assert!(matched("tool(a) ..> tool(b) within 1s", "T:a L T:b").is_empty());
        assert_eq!(
            matched("tool(a) ..> tool(b) within 2s", "T:a L T:b"),
            [(0, 2, vec![0, 2])]
        );
    }

    #[test]
    fn gaps_take_predicates_and_bounds() {
        assert!(matched("tool(a) ..[kind=llm]> tool(b)", "T:a T:c T:b").is_empty());
        assert_eq!(
            matched("tool(a) ..[kind=llm]> tool(b)", "T:a L L T:b"),
            [(0, 3, vec![0, 3])]
        );
        assert!(matched("tool(a) ..{,1}> tool(b)", "T:a L L T:b").is_empty());
        assert_eq!(
            matched("tool(a) ..{2,}> tool(b)", "T:a L T:b L T:b"),
            [(0, 4, vec![0, 4])]
        );
    }

    #[test]
    fn the_path_cap_is_reported_not_silent() {
        // One path per distinct file, each waiting forever for its pair.
        let spec: Vec<String> = (0..(MAX_THREADS + 50))
            .map(|i| format!("T:e@f{i}"))
            .collect();
        let spans = spans(&spec.join(" "));
        let steps = steps(&spans);
        let query = query("edit[file=@f] ..> edit[file=@f]");
        let scan = query.program.scan(&steps, 10, 10, None).expect("scans");
        assert!(scan.inexact, "dropping paths must be flagged");
        assert_eq!(scan.count, 0);
    }

    #[test]
    fn a_full_list_remembers_nothing_new_and_says_so() {
        let query = query("tool[file=@f] (..> tool(a) | ..> tool(b))");
        let program = &query.program;
        let thread = |n: usize| Thread {
            pc: 1,
            env: Env::new(1).with_slot_for_tests(0, &format!("f{n}")),
            start: 0,
            path: None,
        };
        // Remembered states are capped: past MAX_SEEN a new one is refused.
        let mut seen = Seen::new(program);
        for n in 0..MAX_SEEN {
            assert_eq!(seen.insert(1, 0, &thread(n).env), Inserted::New);
        }
        assert_eq!(seen.insert(1, 0, &thread(0).env), Inserted::Known);
        assert_eq!(seen.insert(1, 0, &thread(MAX_SEEN).env), Inserted::Full);

        // A list that cannot remember a state drops it, and says so.
        let mut list = List {
            threads: Vec::new(),
            seen,
        };
        let mut meter = Meter::new(None);
        program
            .add(&mut list, thread(MAX_SEEN + 1), 1, 2, &mut meter)
            .expect("no deadline");
        assert!(list.threads.is_empty());
        assert!(meter.dropped);

        // A list whose paths are full takes no more, remembers nothing for
        // them, and says so.
        let mut list = List {
            threads: (0..MAX_THREADS).map(thread).collect(),
            seen: Seen::new(program),
        };
        let mut meter = Meter::new(None);
        program
            .add(&mut list, thread(MAX_THREADS), 1, 2, &mut meter)
            .expect("no deadline");
        assert_eq!(list.threads.len(), MAX_THREADS);
        assert!(meter.dropped);
        let Seen::Hashed(set) = &list.seen else {
            panic!("captures key the states");
        };
        assert!(set.is_empty(), "nothing remembered for a dropped state");
    }

    #[test]
    fn the_deadline_stops_a_long_scan() {
        let spec = vec!["L"; 5000].join(" ");
        let spans = spans(&spec);
        let steps = steps(&spans);
        let query = query("llm ..> tool(never)");
        let expired =
            Deadline::starting(std::time::Instant::now(), Some(std::time::Duration::ZERO));
        match query.program.scan(&steps, 1, 1, expired) {
            Err(crate::Error::DeadlineExceeded(_)) => {}
            other => panic!("expected a deadline refusal, got {other:?}"),
        }
    }

    // ------------------------------------------------- property-style tests

    struct Rng(u64);

    impl Rng {
        fn next(&mut self, bound: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % bound
        }
        fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
            items[self.next(items.len() as u64) as usize]
        }
    }

    fn random_atom(rng: &mut Rng) -> String {
        rng.pick(&[
            "tool(a)",
            "tool(b)",
            "llm",
            "step",
            "tool[error]",
            "step[not error]",
            "edit[file=@f]",
            "edit[file=@f]",
            "tool(@t)",
            "tool(@t)[error]",
            "agent(@g)",
            "agent[name!=@g]",
            "tool(a|b)[status=ok]",
            "step[kind=tool, name~\"^b\"]",
            "edit(x)",
        ])
        .to_owned()
    }

    fn random_pattern(rng: &mut Rng, depth: u32) -> String {
        if depth == 0 {
            return random_atom(rng);
        }
        let sub = |rng: &mut Rng| random_pattern(rng, depth - 1);
        match rng.next(12) {
            0 | 1 => format!("{} -> {}", sub(rng), sub(rng)),
            2 | 3 => {
                let gap = rng.pick(&["..>", "..[kind=llm]>", "..{,2}>", "..[not error]{1,}>"]);
                format!("{} {gap} {}", sub(rng), sub(rng))
            }
            4 => format!("({} | {})", sub(rng), sub(rng)),
            5 => {
                let quantifier = rng.pick(&["+", "*", "?", "{1,2}", "{2}", "+?", "*?", "{1,3}?"]);
                format!("({}){quantifier}", sub(rng))
            }
            6 => {
                let gap = rng.pick(&["..>", "->", "..[kind=llm]>", "..{1,2}>"]);
                let quantifier = rng.pick(&["", "+", "{2}", "{1,2}?", "*"]);
                let first = sub(rng);
                format!("{first} ({gap} {}){quantifier}", sub(rng))
            }
            11 => format!("^{}", sub(rng)),
            7 => format!("{}$", sub(rng)),
            _ => random_atom(rng),
        }
    }

    fn random_spec(rng: &mut Rng) -> String {
        let length = rng.next(9) as usize;
        (0..length)
            .map(|_| {
                rng.pick(&[
                    "T:a", "T:a!", "T:b", "T:b!", "L", "L!", "A:x", "A:y", "T:e@x", "T:e@y",
                    "T:e@x!",
                ])
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The matcher against the backtracking reference: same matches, same
    /// paths, same captures, on random patterns over random runs.
    #[test]
    fn the_matcher_agrees_with_a_backtracking_reference() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        let mut compared = 0;
        let mut matches = 0;
        for _ in 0..4000 {
            let mut pattern = random_pattern(&mut rng, 3);
            match rng.next(6) {
                0 => pattern.push_str(" within 3"),
                1 => pattern.push_str(" within 2s"),
                _ => {}
            }
            let Ok(query) = Query::compile(&pattern) else {
                // Nullable patterns are refused by design; skip them.
                continue;
            };
            for _ in 0..4 {
                let spans = spans(&random_spec(&mut rng));
                let steps = steps(&spans);
                let scan = query
                    .program
                    .scan(&steps, usize::MAX, usize::MAX, None)
                    .expect("scans");
                let reference = reference_scan(&query, &steps);
                assert!(!scan.inexact);
                assert_eq!(
                    scan.found,
                    reference,
                    "pattern `{pattern}` on {:?}",
                    spans
                        .iter()
                        .map(|s| (&s.attributes, &s.status))
                        .collect::<Vec<_>>()
                );
                compared += 1;
                matches += reference.len();
            }
        }
        assert!(compared > 8000, "only {compared} comparisons ran");
        assert!(
            matches > 2000,
            "the generator must produce matches too ({matches})"
        );
    }

    /// Rendering is canonical: parsing a rendering yields the same tree, and
    /// rendering is idempotent.
    #[test]
    fn rendering_round_trips() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..2000 {
            let pattern = random_pattern(&mut rng, 3);
            let Ok(query) = Query::compile(&pattern) else {
                continue;
            };
            let rendered = query.normalized();
            let again = Query::compile(&rendered)
                .unwrap_or_else(|error| panic!("`{rendered}` (from `{pattern}`) fails: {error}"));
            assert_eq!(
                again.node, query.node,
                "`{pattern}` rendered as `{rendered}`"
            );
            assert_eq!(again.normalized(), rendered);
        }
    }

    #[test]
    fn mutation_check_the_reference_notices_a_wrong_priority() {
        // Guard on the property test itself: a greedy and a lazy pattern
        // must report different paths, so a matcher that ignored laziness
        // could not pass by accident.
        let greedy = matched("tool(a)+ ..> llm", "T:a T:a L");
        let lazy = matched("tool(a)+? ..> llm", "T:a T:a L");
        assert_eq!(greedy, [(0, 2, vec![0, 1, 2])]);
        assert_eq!(lazy, [(0, 2, vec![0, 2])]);
    }
}
