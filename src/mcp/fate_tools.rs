//! MCP tool over the Fate Map: foresight for one run.
//!
//! A facade over [`Store::foresee`](crate::Store::foresee) — the call `GET
//! /v1/fate/runs/{run}` makes — under the module's rules: the headline is
//! numbers and verdict words the server computed; every stored string (the
//! run id, the agent, the path's tool and model names, the explanation that
//! quotes them) renders inside the untrusted-telemetry block or, in
//! structured content, made JSON-safe; the result is clamped to the server's
//! ceiling with the loss stated. A read: nothing here changes a run.

use serde_json::{json, Map, Value};

use super::{
    clamp_report_narrowing, json_safe, known_keys, optional_string, optional_usize, required_str,
    sanitize, serialized_len, tool, Context, Nature, Server, Tool, ToolError, ToolResult,
};
use crate::fate::{Foresight, ForesightVerdict, DEFAULT_VERDICT_SUPPORT};

/// What to do about a result the ceiling cut short.
const FORESEE_NARROW: &str = "The path is listed in full first; the rest is detail.";

/// The tool definitions, appended to the server's read tools.
pub(super) fn definitions() -> Vec<Tool> {
    vec![tool(
        "foresee_run",
        "Foresee a run's fate",
        "How have runs like this one ended? Reads the run's behaviour so far — its tool and \
         model calls in order, repeats folded — and walks it down its agent's Fate Map, the \
         tree of every path the agent's completed runs took. Answers what share of runs that \
         took the same path succeeded (with a 95% interval and how many runs that is), a \
         verdict (on_track, at_risk, likely_failure, novel when no earlier run did what this \
         one just did, insufficient_history), where the run left the known paths, and what \
         earlier runs did next from here and how that went for them. Use it before a costly \
         next step, or when a run seems stuck, to learn from every earlier run instead of \
         guessing. The run itself is left out of the statistics it is judged by.",
        json!({
            "type": "object",
            "properties": {
                "run_id": {
                    "type": "string",
                    "description": "A session id, or 'trace:<trace_id>' for one trace.",
                },
                "tenant": {
                    "type": "string",
                    "description": "The run's tenant, for an unbound credential. Default: the \
                                    default tenant.",
                },
                "min_support": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 100000,
                    "description": format!(
                        "Earlier runs that must share the path before a verdict is given. \
                         Default {DEFAULT_VERDICT_SUPPORT}."
                    ),
                },
            },
            "required": ["run_id"],
            "additionalProperties": false,
        }),
        Nature::Read,
    )
    .with_output_schema(output_schema())]
}

/// Whether `name` is one of this module's tools.
pub(super) fn handles(name: &str) -> bool {
    name == "foresee_run"
}

/// Calls one of this module's tools.
pub(super) fn call(
    server: &Server<'_>,
    _name: &str,
    arguments: &Map<String, Value>,
    context: &Context,
) -> ToolResult {
    known_keys(
        arguments,
        &["run_id", "tenant", "min_support"],
        "foresee_run",
    )?;
    let run = required_str(arguments, "run_id")?;
    let tenant = match (context.scope(), optional_string(arguments, "tenant")?) {
        (Some(bound), Some(named)) if named != bound => {
            return Err(ToolError(
                "This credential is bound to a different tenant; omit 'tenant'.".to_owned(),
            ))
        }
        (Some(bound), _) => bound.to_owned(),
        (None, named) => named.unwrap_or_default(),
    };
    let min_support = optional_usize(arguments, "min_support")?
        .map_or(DEFAULT_VERDICT_SUPPORT, |support| support.max(1) as u64);
    let foresight = server
        .store
        .foresee(&tenant, run, min_support, context.now_ns)?
        .ok_or_else(|| {
            ToolError(
                "No run with that id. A run id is a session id (take one from list_sessions) \
                 or 'trace:' followed by a trace id."
                    .to_owned(),
            )
        })?;
    Ok(render(&foresight, server.limits.max_result_bytes))
}

fn verdict_word(verdict: ForesightVerdict) -> &'static str {
    match verdict {
        ForesightVerdict::OnTrack => "ON TRACK",
        ForesightVerdict::AtRisk => "AT RISK",
        ForesightVerdict::LikelyFailure => "LIKELY FAILURE",
        ForesightVerdict::Novel => "NOVEL",
        ForesightVerdict::InsufficientHistory => "INSUFFICIENT HISTORY",
    }
}

fn render(foresight: &Foresight, max_bytes: usize) -> Value {
    // The headline carries only what the server computed: counts, rates
    // and the verdict. Names live in the telemetry block below.
    let odds = match (foresight.p_success, foresight.ci95, foresight.node.as_ref()) {
        (Some(p), Some(ci), Some(node)) => format!(
            " {} of {} earlier runs that took this path succeeded ({:.1}%, 95% CI {:.1}–{:.1}%).",
            node.success,
            node.visits,
            p * 100.0,
            ci[0] * 100.0,
            ci[1] * 100.0
        ),
        _ => String::new(),
    };
    let base = foresight
        .base_rate
        .map(|base| {
            format!(
                " The agent's {} completed runs succeed {:.0}% of the time.",
                foresight.cohort_runs,
                base * 100.0
            )
        })
        .unwrap_or_default();
    let head = format!(
        "Foresight: {}. {} step(s) recorded; {} of {} path token(s) found in the Fate \
         Map.{odds}{base}",
        verdict_word(foresight.verdict),
        foresight.steps,
        foresight.matched_depth,
        foresight.path.len(),
    );
    let mut rows = vec![
        format!(
            "run {} · agent {} · outcome so far {}",
            sanitize(&foresight.run),
            sanitize(&foresight.agent),
            json!(foresight.outcome).as_str().unwrap_or("unknown")
        ),
        format!("path: {}", sanitize(foresight.path.join(" → "))),
        format!("why: {}", sanitize(&foresight.explanation)),
    ];
    if let Some(divergence) = &foresight.divergence {
        rows.push(format!(
            "left the known paths after {} token(s): took {}; earlier runs went on with {}",
            divergence.depth,
            sanitize(&divergence.took),
            sanitize(divergence.common_next.join(", "))
        ));
    }
    for option in &foresight.next {
        rows.push(format!(
            "next: {}{} — {} runs, {:.0}% succeeded (95% CI {:.0}–{:.0}%)",
            sanitize(&option.token),
            if option.repeat { " again" } else { "" },
            option.visits,
            option.success_rate * 100.0,
            option.ci95[0] * 100.0,
            option.ci95[1] * 100.0
        ));
    }
    if let Some(recommended) = &foresight.recommended_next {
        rows.push(format!(
            "runs that next did {} did best from here",
            sanitize(recommended)
        ));
    }
    let mut notes = vec![
        "Foresight is counting, not a model: the share of this agent's completed runs (last 30 \
         days) whose behaviour started the same way and succeeded. Repeats of a step fold into \
         buckets (×2, ×3-5, ×6+). A completed run is left out of the counts it is judged by."
            .to_owned(),
    ];
    if foresight.last_open {
        notes.push(
            "The run is still going, so its last step may repeat: it is compared with every \
             earlier run that repeated that step at least as often here."
                .to_owned(),
        );
    }
    if foresight.partial {
        notes.push(
            "The run is longer than foresight reads; its path is its beginning, and its outcome \
             is unknown."
                .to_owned(),
        );
    }
    let text = clamp_report_narrowing(&head, &rows, &notes, max_bytes, FORESEE_NARROW);
    let mut structured = structured(foresight);
    let mut result = json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": structured.clone(),
        "isError": false,
    });
    // The structured half travels whatever the ceiling, because the tool
    // advertises a schema for it; what gives way inside it is the long
    // free text — the path and the options.
    if serialized_len(&result) > max_bytes {
        if let Some(object) = structured.as_object_mut() {
            object.insert("next".to_owned(), json!([]));
            if let Some(Value::Array(path)) = object.get_mut("path") {
                path.truncate(8);
            }
        }
        result["structuredContent"] = structured;
    }
    result
}

fn structured(foresight: &Foresight) -> Value {
    let mut value = serde_json::to_value(foresight).unwrap_or_else(|_| json!({}));
    if let Some(object) = value.as_object_mut() {
        for key in ["run", "tenant", "agent", "explanation", "recommended_next"] {
            if let Some(Value::String(text)) = object.get(key) {
                let safe = json_safe(text);
                object.insert(key.to_owned(), json!(safe));
            }
        }
        if let Some(Value::Array(path)) = object.get_mut("path") {
            for token in path.iter_mut() {
                if let Value::String(text) = token {
                    *text = json_safe(text);
                }
            }
        }
        if let Some(Value::Object(node)) = object.get_mut("node") {
            if let Some(Value::String(text)) = node.get_mut("token") {
                *text = json_safe(text);
            }
            if let Some(Value::Array(pooled)) = node.get_mut("pooled") {
                for token in pooled.iter_mut() {
                    if let Value::String(text) = token {
                        *text = json_safe(text);
                    }
                }
            }
        }
        if let Some(Value::Object(divergence)) = object.get_mut("divergence") {
            if let Some(Value::String(text)) = divergence.get_mut("took") {
                *text = json_safe(text);
            }
            if let Some(Value::Array(common)) = divergence.get_mut("common_next") {
                for token in common.iter_mut() {
                    if let Value::String(text) = token {
                        *text = json_safe(text);
                    }
                }
            }
        }
        if let Some(Value::Array(next)) = object.get_mut("next") {
            for option in next.iter_mut() {
                if let Some(Value::String(text)) = option.get_mut("token") {
                    *text = json_safe(text);
                }
            }
        }
    }
    value
}

fn output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "run": {"type": "string"},
            "agent": {"type": "string"},
            "path": {"type": "array", "items": {"type": "string"}},
            "steps": {"type": "integer"},
            "matched_depth": {"type": "integer"},
            "p_success": {"type": ["number", "null"]},
            "ci95": {"type": ["array", "null"], "items": {"type": "number"}},
            "support": {"type": "integer"},
            "base_rate": {"type": ["number", "null"]},
            "verdict": {
                "type": "string",
                "enum": ["on_track", "at_risk", "likely_failure", "novel", "insufficient_history"],
            },
            "explanation": {"type": "string"},
            "divergence": {"type": "object"},
            "next": {"type": "array"},
            "outcome": {"type": "string"},
        },
        "required": ["run", "agent", "path", "verdict", "explanation", "support", "next"],
    })
}

#[cfg(test)]
mod tests {
    use super::super::{Access, Limits, TELEMETRY_CLOSE, TELEMETRY_OPEN};
    use super::*;
    use crate::tape::tests::TempStore;

    #[test]
    fn foresight_renders_names_inside_the_telemetry_frame() {
        let fixture = TempStore::open("mcp-foresee", crate::Config::default());
        let store = fixture.store();
        let now = crate::mcp::unix_nanos_now();
        let corpus = crate::seed::behaviour_corpus(&crate::seed::BehaviourOptions {
            now_ns: now,
            live: true,
            ..crate::seed::BehaviourOptions::default()
        });
        store.ingest_batch(corpus.spans).expect("ingests");
        let server = Server::new(
            store,
            Limits {
                max_result_bytes: 16 * 1024,
                max_payload_bytes: 4096,
            },
            false,
        );
        let response = server
            .handle(
                &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": {"name": "foresee_run",
                               "arguments": {"run_id": "live-doomed-0"}}}),
                Context {
                    access: Access::Read,
                    tenant: None,
                    now_ns: now,
                },
            )
            .expect("a response");
        let result = &response["result"];
        assert_eq!(result["isError"], false, "{response}");
        let text = result["content"][0]["text"].as_str().expect("text");
        let open = text.find(TELEMETRY_OPEN).expect("framed");
        let close = text.find(TELEMETRY_CLOSE).expect("closed");
        let head = &text[..open];
        assert!(head.contains("LIKELY FAILURE"), "{text}");
        assert!(
            !head.contains("search_orders"),
            "a tool name is producer text and must not reach the headline: {head}"
        );
        assert!(
            text[open..close].contains("tool:search_orders×3-5"),
            "{text}"
        );
        assert_eq!(result["structuredContent"]["verdict"], "likely_failure");

        let missing = server
            .handle(
                &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                    "params": {"name": "foresee_run", "arguments": {"run_id": "nope"}}}),
                Context {
                    access: Access::Read,
                    tenant: None,
                    now_ns: now,
                },
            )
            .expect("a response");
        assert_eq!(missing["result"]["isError"], true);

        let foreign = server
            .handle(
                &json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                    "params": {"name": "foresee_run",
                               "arguments": {"run_id": "live-doomed-0", "tenant": "other"}}}),
                Context {
                    access: Access::Read,
                    tenant: Some("acme".to_owned()),
                    now_ns: now,
                },
            )
            .expect("a response");
        assert_eq!(foreign["result"]["isError"], true);
        assert!(foreign["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("bound to a different tenant"));
    }
}
