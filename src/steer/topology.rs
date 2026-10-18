//! Run topology: which agents a run involved, who called whom, who handed
//! control to whom, and where two of them passed it back and forth.
//!
//! Read from the run's stored spans, under the query deadline:
//!
//! - **Nodes** are agents. A span's agent is the agent it names
//!   ([`crate::semconv::StepFacts::agent`]); a span naming none belongs to
//!   its parent's agent when the parent ran in the same service, and is its
//!   own service's otherwise — a process boundary is an actor of its own.
//!   `named` says whether a node's identity was named by telemetry or is a
//!   service standing in for one.
//! - **Edges** are parent → child span relations whose two spans belong to
//!   different agents: a delegation, counted with its errors (child spans
//!   with status `error`) and the first and last child start.
//! - **Handoffs** and **cycles** are the run's handoff sequence — steps that
//!   name an agent, in step order, consecutive steps of one agent merged
//!   into a turn — and its `A → B → A` returns, exactly as
//!   [`super::handoff`] defines them for the guard's `max_handoff_cycles`.
//!   A guard holds a run to its cycles since its last reset; the topology
//!   counts the whole run it read.
//!
//! Bounded: at most [`MAX_TOPOLOGY_SPANS`] spans are read (the earliest, in
//! step order — `truncated` says so), and the answer keeps at most
//! [`MAX_NODES`] nodes, [`MAX_EDGES`] edges, [`MAX_CYCLES`] cycles and
//! [`MAX_SEQUENCE`] turns of the sequence, the largest first, each list
//! saying when it was cut.

use std::collections::HashMap;
use std::time::Instant;

use serde::Serialize;

use super::handoff::{self, clip_name, cycles_in};
use crate::guard::SpanRef;
use crate::semconv::{self, StepKind};
use crate::{Deadline, Result, Store};

/// Most spans one topology reads.
pub const MAX_TOPOLOGY_SPANS: usize = 50_000;

/// Most nodes returned.
pub const MAX_NODES: usize = 256;

/// Most edges returned.
pub const MAX_EDGES: usize = 1024;

/// Most cycle pairs returned.
pub const MAX_CYCLES: usize = 256;

/// Most turns of the handoff sequence returned.
pub const MAX_SEQUENCE: usize = 500;

/// Spans extracted between deadline checks.
const DEADLINE_STRIDE: usize = 1024;

/// One agent of a run.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TopologyNode {
    /// The agent's name, or the service standing in for one.
    pub agent: String,
    /// Whether telemetry named it as an agent (`false`: a service).
    pub named: bool,
    /// Spans of the run that belong to it.
    pub spans: u64,
    /// Of those, steps: LLM, tool and agent spans.
    pub steps: u64,
    /// LLM steps.
    pub llm_calls: u64,
    /// Tool steps.
    pub tool_calls: u64,
    /// Spans with status `error`.
    pub errors: u64,
    /// Priced cost, USD.
    pub cost_usd: f64,
    /// Tokens.
    pub tokens: u64,
    /// Its earliest span start.
    pub first_ns: u64,
    /// Its latest span end.
    pub last_ns: u64,
}

/// Spans of one agent whose parents belong to another: a delegation.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TopologyEdge {
    /// The parent spans' agent.
    pub from: String,
    /// The child spans' agent.
    pub to: String,
    /// Parent → child relations.
    pub count: u64,
    /// Of those, children with status `error`.
    pub errors: u64,
    /// The earliest child start.
    pub first_ns: u64,
    /// The latest child start.
    pub last_ns: u64,
}

/// Control passing from one named agent's turn to another's.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TopologyHandoff {
    /// The agent whose turn ended.
    pub from: String,
    /// The agent whose turn began.
    pub to: String,
    /// How often.
    pub count: u64,
}

/// Returns of control to one agent straight from another: turns
/// `agents[0], agents[1], agents[0]`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TopologyCycle {
    /// The agent control returned to, then the agent it returned from.
    pub agents: [String; 2],
    /// How often.
    pub count: u64,
    /// Start of the first step of the earliest return.
    pub first_ns: u64,
    /// Start of the first step of the latest return.
    pub last_ns: u64,
    /// That step.
    pub example: SpanRef,
}

/// A run's agent graph.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Topology {
    /// The run id.
    pub run: String,
    /// The run's tenant; empty is the default tenant.
    pub tenant: String,
    /// The agents, most spans first.
    pub nodes: Vec<TopologyNode>,
    /// Whether nodes were cut to [`MAX_NODES`].
    pub nodes_truncated: bool,
    /// Delegations, most frequent first.
    pub edges: Vec<TopologyEdge>,
    /// Whether edges were cut to [`MAX_EDGES`].
    pub edges_truncated: bool,
    /// Turn-to-turn handoffs between named agents, most frequent first.
    pub handoffs: Vec<TopologyHandoff>,
    /// The handoff sequence: one entry per turn, in order.
    pub sequence: Vec<String>,
    /// Whether the sequence was cut to [`MAX_SEQUENCE`] turns.
    pub sequence_truncated: bool,
    /// Ping-pong: returns per ordered pair, most first.
    pub cycles: Vec<TopologyCycle>,
    /// Spans read.
    pub spans_read: usize,
    /// Whether the run had more than [`MAX_TOPOLOGY_SPANS`] spans, so all
    /// of the above describes its earliest ones.
    pub truncated: bool,
}

/// One span reduced to what the topology needs.
struct Reduced {
    agent: Option<String>,
    service: String,
    parent: Option<usize>,
    kind: StepKind,
    error: bool,
    cost_usd: f64,
    tokens: u64,
    start_ns: u64,
    end_ns: u64,
    trace_id: String,
    span_id: String,
}

impl Store {
    /// `run`'s topology within `tenant` (empty: the default tenant); see
    /// the module docs. A run no span carries is an empty topology.
    pub fn run_topology(&self, tenant: &str, run: &str) -> Result<Topology> {
        crate::guard::validate_tenant(tenant)?;
        crate::guard::validate_run(run)?;
        let deadline = self.query_deadline(Instant::now());
        let stored = self.run_spans(Some(tenant), run, MAX_TOPOLOGY_SPANS, deadline)?;
        let mut index: HashMap<(&str, &str), usize> = HashMap::with_capacity(stored.spans.len());
        for (position, span) in stored.spans.iter().enumerate() {
            index.insert((span.trace_id.as_str(), span.span_id.as_str()), position);
        }
        let mut reduced = Vec::with_capacity(stored.spans.len());
        for (position, span) in stored.spans.iter().enumerate() {
            if position % DEADLINE_STRIDE == 0 {
                Deadline::check(deadline, 0)?;
            }
            let llm = self.facts(span);
            let parent = span
                .parent_span_id
                .as_deref()
                .and_then(|parent| index.get(&(span.trace_id.as_str(), parent)).copied())
                .filter(|parent| *parent != position);
            reduced.push(Reduced {
                agent: semconv::span_agent(span).map(|agent| clip_name(&agent)),
                service: clip_name(&span.service),
                parent,
                kind: semconv::step_kind_with(span, &llm),
                error: span.status == "error",
                cost_usd: llm.cost_usd.filter(|cost| cost.is_finite()).unwrap_or(0.0),
                tokens: llm.total(),
                start_ns: span.start_time_ns,
                end_ns: span.end_time_ns,
                trace_id: span.trace_id.clone(),
                span_id: span.span_id.clone(),
            });
        }
        Deadline::check(deadline, 0)?;
        Ok(assemble(run, tenant, &reduced, stored.truncated))
    }
}

/// Every span's agent: named, else its parent's within one service, else
/// its service. Resolved without recursion, and a parent loop in malformed
/// telemetry ends at the span that closes it.
fn agents_of(spans: &[Reduced]) -> Vec<(String, bool)> {
    let mut resolved: Vec<Option<(String, bool)>> = vec![None; spans.len()];
    for start in 0..spans.len() {
        if resolved[start].is_some() {
            continue;
        }
        // Walk up until a span whose agent is known or decided locally.
        let mut chain = Vec::new();
        let mut at = start;
        let found = loop {
            if let Some(known) = &resolved[at] {
                break known.clone();
            }
            let span = &spans[at];
            if let Some(agent) = &span.agent {
                break (agent.clone(), true);
            }
            match span.parent {
                Some(parent)
                    if spans[parent].service == span.service
                        && !chain.contains(&parent)
                        && parent != start =>
                {
                    chain.push(at);
                    at = parent;
                }
                _ => break (span.service.clone(), false),
            }
        };
        resolved[at] = Some(found.clone());
        for link in chain {
            resolved[link] = Some(found.clone());
        }
    }
    resolved
        .into_iter()
        .map(|agent| agent.unwrap_or_default())
        .collect()
}

fn assemble(run: &str, tenant: &str, spans: &[Reduced], truncated: bool) -> Topology {
    let agents = agents_of(spans);

    let mut nodes: HashMap<&str, TopologyNode> = HashMap::new();
    for (span, (agent, named)) in spans.iter().zip(&agents) {
        let node = nodes.entry(agent.as_str()).or_insert_with(|| TopologyNode {
            agent: agent.clone(),
            named: false,
            spans: 0,
            steps: 0,
            llm_calls: 0,
            tool_calls: 0,
            errors: 0,
            cost_usd: 0.0,
            tokens: 0,
            first_ns: u64::MAX,
            last_ns: 0,
        });
        node.named |= *named;
        node.spans += 1;
        node.steps += u64::from(span.kind.is_step());
        node.llm_calls += u64::from(span.kind == StepKind::Llm);
        node.tool_calls += u64::from(span.kind == StepKind::Tool);
        node.errors += u64::from(span.error);
        node.cost_usd += span.cost_usd;
        node.tokens = node.tokens.saturating_add(span.tokens);
        node.first_ns = node.first_ns.min(span.start_ns);
        node.last_ns = node.last_ns.max(span.end_ns);
    }
    let mut nodes: Vec<TopologyNode> = nodes.into_values().collect();
    nodes.sort_by(|left, right| {
        right
            .spans
            .cmp(&left.spans)
            .then_with(|| left.agent.cmp(&right.agent))
    });
    let nodes_truncated = nodes.len() > MAX_NODES;
    nodes.truncate(MAX_NODES);

    let mut edges: HashMap<(&str, &str), TopologyEdge> = HashMap::new();
    for (position, span) in spans.iter().enumerate() {
        let Some(parent) = span.parent else { continue };
        let (from, to) = (agents[parent].0.as_str(), agents[position].0.as_str());
        if from == to {
            continue;
        }
        let edge = edges.entry((from, to)).or_insert_with(|| TopologyEdge {
            from: from.to_owned(),
            to: to.to_owned(),
            count: 0,
            errors: 0,
            first_ns: u64::MAX,
            last_ns: 0,
        });
        edge.count += 1;
        edge.errors += u64::from(span.error);
        edge.first_ns = edge.first_ns.min(span.start_ns);
        edge.last_ns = edge.last_ns.max(span.start_ns);
    }
    let mut edges: Vec<TopologyEdge> = edges.into_values().collect();
    edges.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| (&left.from, &left.to).cmp(&(&right.from, &right.to)))
    });
    let edges_truncated = edges.len() > MAX_EDGES;
    edges.truncate(MAX_EDGES);

    // The handoff sequence, in the guard's own step order: start, then the
    // span key the guard ties on, so the two cannot order a tie differently.
    let mut named: Vec<(handoff::Position, usize)> = spans
        .iter()
        .enumerate()
        .filter(|(_, span)| span.kind.is_step() && span.agent.is_some())
        .map(|(position, span)| {
            (
                (
                    span.start_ns,
                    crate::guard::span_key(&span.trace_id, &span.span_id),
                ),
                position,
            )
        })
        .collect();
    named.sort();
    // Turns: the agent, and the span that opened the turn.
    let mut turns: Vec<(&str, usize)> = Vec::new();
    for (_, position) in &named {
        let agent = spans[*position].agent.as_deref().unwrap_or_default();
        if turns.last().map(|(last, _)| *last) != Some(agent) {
            turns.push((agent, *position));
        }
    }
    let mut handoffs: HashMap<(&str, &str), u64> = HashMap::new();
    for pair in turns.windows(2) {
        *handoffs.entry((pair[0].0, pair[1].0)).or_default() += 1;
    }
    let mut handoffs: Vec<TopologyHandoff> = handoffs
        .into_iter()
        .map(|((from, to), count)| TopologyHandoff {
            from: from.to_owned(),
            to: to.to_owned(),
            count,
        })
        .collect();
    handoffs.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| (&left.from, &left.to).cmp(&(&right.from, &right.to)))
    });

    let names: Vec<&str> = turns.iter().map(|(agent, _)| *agent).collect();
    let counts = cycles_in(&names);
    let mut cycles: Vec<TopologyCycle> = Vec::with_capacity(counts.len());
    for ((to, from), count) in counts {
        let returns: Vec<usize> = names
            .windows(3)
            .enumerate()
            .filter(|(_, window)| window[0] == to && window[1] == from && window[2] == to)
            .map(|(index, _)| turns[index + 2].1)
            .collect();
        let (Some(first), Some(last)) = (returns.first(), returns.last()) else {
            continue;
        };
        cycles.push(TopologyCycle {
            agents: [to.to_owned(), from.to_owned()],
            count,
            first_ns: spans[*first].start_ns,
            last_ns: spans[*last].start_ns,
            example: SpanRef {
                trace_id: spans[*last].trace_id.clone(),
                span_id: spans[*last].span_id.clone(),
            },
        });
    }
    cycles.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.agents.cmp(&right.agents))
    });
    cycles.truncate(MAX_CYCLES);

    let sequence_truncated = names.len() > MAX_SEQUENCE;
    let sequence = names
        .iter()
        .take(MAX_SEQUENCE)
        .map(|name| (*name).to_owned())
        .collect();

    Topology {
        run: run.to_owned(),
        tenant: tenant.to_owned(),
        nodes,
        nodes_truncated,
        edges,
        edges_truncated,
        handoffs,
        sequence,
        sequence_truncated,
        cycles,
        spans_read: spans.len(),
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(
        id: &str,
        parent: Option<usize>,
        agent: Option<&str>,
        service: &str,
        kind: StepKind,
        start: u64,
    ) -> Reduced {
        Reduced {
            agent: agent.map(str::to_owned),
            service: service.to_owned(),
            parent,
            kind,
            error: false,
            cost_usd: 0.0,
            tokens: 0,
            start_ns: start,
            end_ns: start + 1,
            trace_id: "t".to_owned(),
            span_id: id.to_owned(),
        }
    }

    #[test]
    fn unnamed_spans_inherit_within_a_service_and_not_across_one() {
        let spans = vec![
            span("root", None, Some("planner"), "app", StepKind::Agent, 0),
            span("llm", Some(0), None, "app", StepKind::Llm, 1),
            span("tool", Some(1), None, "app", StepKind::Tool, 2),
            span("http", Some(2), None, "search-svc", StepKind::Other, 3),
            span("db", Some(3), None, "search-svc", StepKind::Other, 4),
            span("orphan", None, None, "app", StepKind::Llm, 5),
        ];
        let agents: Vec<String> = agents_of(&spans).into_iter().map(|(a, _)| a).collect();
        assert_eq!(
            agents,
            [
                "planner",
                "planner",
                "planner",
                "search-svc",
                "search-svc",
                "app"
            ]
        );
        let topology = assemble("r", "", &spans, false);
        assert_eq!(topology.edges.len(), 1, "{:?}", topology.edges);
        assert_eq!(
            (
                topology.edges[0].from.as_str(),
                topology.edges[0].to.as_str()
            ),
            ("planner", "search-svc")
        );
        let planner = topology
            .nodes
            .iter()
            .find(|node| node.agent == "planner")
            .expect("planner");
        assert!(planner.named && planner.spans == 3 && planner.steps == 3);
        assert!(
            !topology
                .nodes
                .iter()
                .find(|node| node.agent == "search-svc")
                .expect("service")
                .named
        );
    }

    #[test]
    fn a_parent_loop_terminates() {
        let spans = vec![
            span("a", Some(1), None, "svc", StepKind::Other, 0),
            span("b", Some(0), None, "svc", StepKind::Other, 1),
        ];
        let agents = agents_of(&spans);
        assert_eq!(agents.len(), 2);
        assert!(agents.iter().all(|(agent, named)| agent == "svc" && !named));
    }

    #[test]
    fn ping_pong_between_sibling_agents_is_counted_per_direction() {
        // A handoff swarm: triage and billing take turns under one root.
        let order = ["triage", "billing", "triage", "billing", "triage"];
        let mut spans = vec![span("root", None, None, "app", StepKind::Other, 0)];
        for (index, agent) in order.iter().enumerate() {
            let start = 10 + index as u64 * 10;
            spans.push(span(
                &format!("agent-{index}"),
                Some(0),
                Some(agent),
                "app",
                StepKind::Agent,
                start,
            ));
            let parent = spans.len() - 1;
            spans.push(span(
                &format!("llm-{index}"),
                Some(parent),
                None,
                "app",
                StepKind::Llm,
                start + 1,
            ));
        }
        let topology = assemble("r", "", &spans, false);
        assert_eq!(topology.sequence, order);
        assert_eq!(topology.cycles.len(), 2);
        assert_eq!(
            topology.cycles[0].agents,
            ["triage".to_owned(), "billing".to_owned()]
        );
        assert_eq!(topology.cycles[0].count, 2);
        assert_eq!(topology.cycles[0].example.span_id, "agent-4");
        assert_eq!(topology.cycles[0].first_ns, 30);
        assert_eq!(topology.cycles[1].count, 1);
        let handoffs: Vec<(String, String, u64)> = topology
            .handoffs
            .iter()
            .map(|handoff| (handoff.from.clone(), handoff.to.clone(), handoff.count))
            .collect();
        assert_eq!(
            handoffs,
            [
                ("billing".to_owned(), "triage".to_owned(), 2),
                ("triage".to_owned(), "billing".to_owned(), 2)
            ]
        );
        // Every agent span hangs off the unnamed root: app → agent edges.
        assert!(topology.edges.iter().all(|edge| edge.from == "app"));
    }
}
