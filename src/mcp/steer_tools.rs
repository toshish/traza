//! MCP tools over steering: the runs that are live now, and one run's agent
//! graph.
//!
//! Facades over [`Store::live_runs`](crate::Store::live_runs) and
//! [`Store::run_topology`](crate::Store::run_topology) — the calls
//! `GET /v1/runs/live` and `GET /v1/runs/{run}/topology` make — under the
//! module's rules: agent names, run ids, tool names and reasons all came
//! from telemetry, so they render inside the untrusted-telemetry block (or,
//! in structured content, made JSON-safe), and results are clamped to the
//! server's ceiling with the loss stated. Both are reads.
//!
//! There is deliberately no nudge tool, in either direction. An agent hears
//! nudges through its pre-call check, never by polling MCP — a nudge read
//! here would be consumed or left unread depending on which surface asked
//! first — and queueing one is a human's act, not something stored
//! telemetry should be able to steer a model into doing.

use serde_json::{json, Map, Value};

use super::{
    budgeted_structured_result_narrowing, clamp_report_narrowing, count, duration_human, json_safe,
    known_keys, optional_string, optional_usize, required_str, sanitize, text_result, thousands,
    tool, Context, Nature, Server, Tool, ToolError, ToolResult,
};
use crate::steer::{LiveRun, Topology};

/// Live runs `list_live_runs` returns when the caller names no limit.
const DEFAULT_LIVE_LIMIT: usize = 20;

/// Live runs it returns at most.
const MAX_LIVE_LIMIT: usize = 200;

/// Turns of the handoff sequence rendered.
const SEQUENCE_SHOWN: usize = 60;

const LIVE_NARROW: (&str, &str) = ("Ask for a smaller 'limit'.", "Ask for a smaller 'limit'.");

const TOPOLOGY_NARROW: &str = "The graph is shown largest first; open single agents' steps \
                               with get_run_tape.";

/// The tool definitions: the topology always, live runs when the server
/// observes runs (`fuses`).
pub(super) fn definitions(fuses: bool) -> Vec<Tool> {
    let mut tools = vec![tool(
        "run_topology",
        "Show a run's agent graph",
        "How a multi-agent run was organized: its agents, which agent delegated work to \
         which (parent and child spans in different agents, with error counts), how control \
         passed from one named agent's turn to the next, and where two agents bounced it back \
         and forth — A → B → A returns, counted per direction. Use it to see why a \
         multi-agent run looped or stalled before reading individual steps with \
         get_run_tape. A run id is a session id, or 'trace:<trace_id>'.",
        json!({
            "type": "object",
            "properties": {
                "run_id": {
                    "type": "string",
                    "description": "A session id, or 'trace:<trace_id>' for one trace.",
                },
                "tenant": {
                    "type": "string",
                    "description": "The run's tenant, for an unbound credential. Default: \
                                    the default tenant.",
                },
            },
            "required": ["run_id"],
            "additionalProperties": false,
        }),
        Nature::Read,
    )];
    if fuses {
        tools.push(tool(
            "list_live_runs",
            "List the runs that are live now",
            "Agent runs with recent activity — a span arriving or a pre-call check — most \
             recently changed first, each with its agent, the step it is on, its steps, \
             cost, tokens and errors, its guard verdict, and how many nudges from a human are \
             waiting for it. From the server's live state: nothing here was re-read from \
             storage, and a run the server began following mid-way shows what it has seen \
             since.",
            json!({
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_LIVE_LIMIT,
                        "description": format!(
                            "Runs to return. Default {DEFAULT_LIVE_LIMIT}, capped at \
                             {MAX_LIVE_LIMIT}."
                        ),
                    },
                    "tenant": {
                        "type": "string",
                        "description": "Only this tenant's runs, for an unbound credential. \
                                        Default: every tenant.",
                    },
                },
                "additionalProperties": false,
            }),
            Nature::Read,
        ));
    }
    tools
}

/// Whether `name` is one of this module's tools.
pub(super) fn handles(name: &str) -> bool {
    matches!(name, "run_topology" | "list_live_runs")
}

/// Calls one of this module's tools.
pub(super) fn call(
    server: &Server<'_>,
    name: &str,
    arguments: &Map<String, Value>,
    context: &Context,
) -> ToolResult {
    match name {
        "run_topology" => run_topology(server, arguments, context),
        _ if !server.store.fuses_enabled() => Err(ToolError(
            "list_live_runs is disabled; the server was started with --no-fuses".to_owned(),
        )),
        _ => list_live_runs(server, arguments, context),
    }
}

/// The tenant one run lives in: the binding, else the named one, else the
/// default tenant.
fn run_tenant(arguments: &Map<String, Value>, context: &Context) -> Result<String, ToolError> {
    match (context.scope(), optional_string(arguments, "tenant")?) {
        (Some(bound), Some(named)) if named != bound => Err(ToolError(
            "This credential is bound to a different tenant; omit 'tenant'.".to_owned(),
        )),
        (Some(bound), _) => Ok(bound.to_owned()),
        (None, named) => Ok(named.unwrap_or_default()),
    }
}

fn tenant_label(tenant: &str) -> String {
    if tenant.is_empty() {
        "default".to_owned()
    } else {
        sanitize(tenant)
    }
}

fn run_topology(
    server: &Server<'_>,
    arguments: &Map<String, Value>,
    context: &Context,
) -> ToolResult {
    known_keys(arguments, &["run_id", "tenant"], "run_topology")?;
    let run = required_str(arguments, "run_id")?;
    let tenant = run_tenant(arguments, context)?;
    let topology = server.store.run_topology(&tenant, run)?;
    if topology.spans_read == 0 {
        return Err(ToolError(
            "No run with that id. A run id is a session id (take one from list_sessions or \
             list_live_runs) or 'trace:' followed by a trace id."
                .to_owned(),
        ));
    }
    Ok(text_result(render_topology(
        &topology,
        server.limits.max_result_bytes,
    )))
}

fn render_topology(topology: &Topology, max_bytes: usize) -> String {
    let top = topology.cycles.first();
    let head = format!(
        "Run {} (tenant {}): {} across {}, {} between them, {} between named agents{}.{}",
        sanitize(&topology.run),
        tenant_label(&topology.tenant),
        count(topology.spans_read as u64, "span"),
        count(topology.nodes.len() as u64, "agent"),
        count(topology.edges.len() as u64, "delegation path"),
        count(
            topology.handoffs.iter().map(|handoff| handoff.count).sum(),
            "handoff"
        ),
        match top {
            Some(cycle) => format!(
                "; the most frequent ping-pong returned control {} (see the rows)",
                count(cycle.count, "time")
            ),
            None => "; no ping-pong".to_owned(),
        },
        if topology.truncated {
            " TRUNCATED: the run has more spans than one topology reads; this describes its \
             earliest."
        } else {
            ""
        },
    );
    let mut rows = Vec::new();
    for node in &topology.nodes {
        rows.push(format!(
            "agent {}{} · {} · {} ({} LLM, {} tool) · {} · ${:.4} · {} tok · {}",
            sanitize(&node.agent),
            if node.named { "" } else { " (a service)" },
            count(node.spans, "span"),
            count(node.steps, "step"),
            node.llm_calls,
            node.tool_calls,
            count(node.errors, "error"),
            node.cost_usd,
            thousands(node.tokens),
            duration_human(node.last_ns.saturating_sub(node.first_ns)),
        ));
    }
    for edge in &topology.edges {
        rows.push(format!(
            "delegation {} → {} ×{}{}",
            sanitize(&edge.from),
            sanitize(&edge.to),
            edge.count,
            if edge.errors > 0 {
                format!(" ({} failed)", edge.errors)
            } else {
                String::new()
            },
        ));
    }
    for cycle in &topology.cycles {
        rows.push(format!(
            "ping-pong {} → {} → {} ×{} (latest return: trace={} span={})",
            sanitize(&cycle.agents[0]),
            sanitize(&cycle.agents[1]),
            sanitize(&cycle.agents[0]),
            cycle.count,
            sanitize(&cycle.example.trace_id),
            sanitize(&cycle.example.span_id),
        ));
    }
    if !topology.sequence.is_empty() {
        let shown: Vec<String> = topology
            .sequence
            .iter()
            .take(SEQUENCE_SHOWN)
            .map(sanitize)
            .collect();
        let more = topology.sequence.len().saturating_sub(SEQUENCE_SHOWN);
        rows.push(format!(
            "turns: {}{}",
            shown.join(" → "),
            if more > 0 || topology.sequence_truncated {
                " → …".to_owned()
            } else {
                String::new()
            }
        ));
    }
    let notes = vec![
        "An agent is the agent a span names; a span naming none belongs to its parent's agent \
         in the same service, else to its service, marked '(a service)'. Turns and ping-pong \
         count only steps that name an agent: a ping-pong A → B → A is control returning to \
         A straight from B. A run fuse's max_handoff_cycles limit counts the same returns."
            .to_owned(),
    ];
    clamp_report_narrowing(&head, &rows, &notes, max_bytes, TOPOLOGY_NARROW)
}

fn list_live_runs(
    server: &Server<'_>,
    arguments: &Map<String, Value>,
    context: &Context,
) -> ToolResult {
    known_keys(arguments, &["limit", "tenant"], "list_live_runs")?;
    let limit = optional_usize(arguments, "limit")?
        .unwrap_or(DEFAULT_LIVE_LIMIT)
        .clamp(1, MAX_LIVE_LIMIT);
    let tenant = match (context.scope(), optional_string(arguments, "tenant")?) {
        (Some(bound), Some(named)) if named != bound => {
            return Err(ToolError(
                "This credential is bound to a different tenant; omit 'tenant'.".to_owned(),
            ))
        }
        (Some(bound), _) => Some(bound.to_owned()),
        (None, named) => named,
    };
    let page = server.store.live_runs(tenant.as_deref(), limit)?;
    let window = duration_human(page.window_ns);
    let head = format!(
        "{} live in the last {window}{}.",
        count(page.runs.len() as u64, "run"),
        if page.truncated {
            format!("; more are live than the limit of {limit} shows")
        } else {
            String::new()
        }
    );
    let rows: Vec<String> = page.runs.iter().map(live_row).collect();
    let structured: Vec<Value> = page.runs.iter().map(live_structured).collect();
    let notes = vec![
        "Verdicts are the run guard's: continue, warn (near a limit) or stop. 'pending' is \
         the step a pre-call check asked about with no span recorded since. Nudges are \
         delivered to the run through its next pre-call check, never through MCP."
            .to_owned(),
    ];
    Ok(budgeted_structured_result_narrowing(
        &head,
        &rows,
        &notes,
        "runs",
        &structured,
        &[("window_ns", json!(page.window_ns))],
        server.limits.max_result_bytes,
        LIVE_NARROW,
    ))
}

fn live_row(run: &LiveRun) -> String {
    let current = match &run.current {
        Some(step) => format!(
            " · on {} {}{}",
            step.kind,
            sanitize(step.name.as_deref().unwrap_or("(unnamed)")),
            if step.pending { " (pending)" } else { "" }
        ),
        None => String::new(),
    };
    format!(
        "run {} (tenant {}) · agent {} · {} · {} · ${:.4} · {} tok · {}{current}{}{}",
        sanitize(&run.run),
        tenant_label(&run.tenant),
        sanitize(run.agent.as_deref().unwrap_or("?")),
        run.verdict.as_str().to_ascii_uppercase(),
        count(run.steps, "step"),
        run.cost_usd,
        thousands(run.tokens),
        count(run.errors, "error"),
        if run.pending_nudges > 0 {
            format!(" · {} waiting", count(run.pending_nudges as u64, "nudge"))
        } else {
            String::new()
        },
        run.reasons
            .first()
            .map(|reason| format!(" · {}", sanitize(reason)))
            .unwrap_or_default(),
    )
}

fn live_structured(run: &LiveRun) -> Value {
    json!({
        "run": json_safe(&run.run),
        "tenant": json_safe(&run.tenant),
        "agent": run.agent.as_deref().map(json_safe),
        "verdict": run.verdict,
        "steps": run.steps,
        "cost_usd": run.cost_usd,
        "tokens": run.tokens,
        "errors": run.errors,
        "last_activity_ns": run.last_activity_ns,
        "pending_nudges": run.pending_nudges,
        "current": run.current.as_ref().map(|step| json!({
            "kind": step.kind,
            "name": step.name.as_deref().map(json_safe),
            "pending": step.pending,
        })),
    })
}
