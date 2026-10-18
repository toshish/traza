//! MCP tool over the Fate Map: foresight for one run.
//!
//! A facade over [`Store::foresee`](crate::Store::foresee) — the call `GET
//! /v1/fate/runs/{run}` makes — under the module's rules: the headline is
//! numbers and verdict words the server computed; every stored string (the
//! run id, the agent, the path's tool and model names, the explanation that
//! quotes them) renders inside the untrusted-telemetry block or, in
//! structured content, made JSON-safe; the whole result — text, structured
//! content and envelope — fits the server's ceiling with the loss stated, or
//! is refused when even its identifiers cannot. A read: nothing here
//! changes a run.

use serde_json::{json, Map, Value};

use super::{
    clamp_report_narrowing, json_safe, known_keys, optional_string, optional_usize, report,
    required_str, sanitize, serialized_len, tool, truncate_bytes, Context, Nature, Server, Tool,
    ToolError, ToolResult,
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
    render(&foresight, server.limits.max_result_bytes)
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

fn render(foresight: &Foresight, max_bytes: usize) -> ToolResult {
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
    // The structured half travels whatever the ceiling, because the tool
    // advertises a schema for it. It is cut, detail first, until it fits
    // beside the headline; what is left of the ceiling is the text's.
    let mut structured = match structured(foresight) {
        Value::Object(object) => object,
        _ => Map::new(),
    };
    let mut cut = Cut::default();
    let head = loop {
        structured.insert("truncated".to_owned(), json!(cut.any()));
        let head = match cut.note(foresight.path.len()) {
            Some(note) => format!("{head} {note}"),
            None => head.clone(),
        };
        if serialized_len(&assemble(&report(&head, &[], &[]), &structured)) <= max_bytes {
            break head;
        }
        if !cut.next(&mut structured) {
            // What is left is the run's identity — its id, tenant and
            // agent — with the verdict and its numbers: no answer without
            // them is an answer about this run.
            return Err(ToolError(format!(
                "The foresight for this run does not fit under this server's \
                 --mcp-max-result-bytes ({max_bytes} bytes) even with every detail left out: \
                 the run id, tenant and agent name alone take too much of it. Raise \
                 --mcp-max-result-bytes, or read it from GET /v1/fate/runs/{{run}}."
            )));
        }
    };
    // The headline alone fits beside that, by the loop above.
    let minimal = report(&head, &[], &[]);
    let skeleton = serialized_len(&assemble("", &structured));
    let mut budget = max_bytes.saturating_sub(skeleton);
    loop {
        // Rows give way first, then the notes; never the headline, which
        // carries the verdict, its numbers and what was cut.
        let text = [notes.as_slice(), &[]]
            .into_iter()
            .map(|notes| clamp_report_narrowing(&head, &rows, notes, budget, FORESEE_NARROW))
            .find(|text| text.starts_with(head.trim_end()))
            .unwrap_or_else(|| minimal.clone());
        let result = assemble(&text, &structured);
        let over = serialized_len(&result).saturating_sub(max_bytes);
        if over == 0 {
            return Ok(result);
        }
        // JSON escaping made the text longer than its bytes: give back
        // what it overran by, and lay it out again. Once the budget is
        // below the headline, the headline alone is what goes.
        budget = budget.saturating_sub(over);
    }
}

fn assemble(text: &str, structured: &Map<String, Value>) -> Value {
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": Value::Object(structured.clone()),
        "isError": false,
    })
}

/// Bytes the explanation is cut to, in turn, before it is left out.
const EXPLANATION_CUTS: [usize; 2] = [256, 64];

/// What a result too large for the ceiling has left out of its structured
/// half so far. Detail goes first — what earlier runs did next, the
/// recommendation, the pooled and common tokens, the divergence and the node
/// — then the explanation, shortened and then emptied, then the path from
/// its end. The run's identity, its verdict and its numbers are never cut:
/// a result that cannot carry them is refused instead.
#[derive(Default)]
struct Cut {
    fields: Vec<&'static str>,
    /// Path tokens kept, once the path has been cut.
    path_kept: Option<usize>,
}

impl Cut {
    fn any(&self) -> bool {
        !self.fields.is_empty() || self.path_kept.is_some()
    }

    /// The sentence the headline carries once anything was cut: server
    /// words and field names only.
    fn note(&self, path_len: usize) -> Option<String> {
        if !self.any() {
            return None;
        }
        let mut left: Vec<String> = self
            .fields
            .iter()
            .map(|field| (*field).to_owned())
            .collect();
        if let Some(kept) = self.path_kept {
            left.push(format!("path past its first {kept} of {path_len} tokens"));
        }
        Some(format!(
            "Truncated to fit this server's --mcp-max-result-bytes: the structured result \
             leaves out {} (truncated: true).",
            left.join(", ")
        ))
    }

    fn left_out(&mut self, field: &'static str) {
        // A whole object left out names itself, not its parts again.
        self.fields.retain(|part| {
            part.split_once('.')
                .map_or(true, |(whole, _)| whole != field)
        });
        if !self.fields.contains(&field) {
            self.fields.push(field);
        }
    }

    /// Cuts the next piece from `structured`; false when nothing is left
    /// that may be cut.
    fn next(&mut self, structured: &mut Map<String, Value>) -> bool {
        let non_empty = |value: Option<&Value>| {
            value
                .and_then(Value::as_array)
                .is_some_and(|items| !items.is_empty())
        };
        if non_empty(structured.get("next")) {
            structured.insert("next".to_owned(), json!([]));
            self.left_out("next");
            return true;
        }
        if structured.remove("recommended_next").is_some() {
            self.left_out("recommended_next");
            return true;
        }
        if let Some(Value::Object(node)) = structured.get_mut("node") {
            if non_empty(node.get("pooled")) {
                node.insert("pooled".to_owned(), json!([]));
                self.left_out("node.pooled");
                return true;
            }
        }
        if let Some(Value::Object(divergence)) = structured.get_mut("divergence") {
            if non_empty(divergence.get("common_next")) {
                divergence.insert("common_next".to_owned(), json!([]));
                self.left_out("divergence.common_next");
                return true;
            }
        }
        if structured.remove("divergence").is_some() {
            self.left_out("divergence");
            return true;
        }
        if structured.get("node").is_some_and(Value::is_object) {
            structured.remove("node");
            self.left_out("node");
            return true;
        }
        if let Some(Value::String(explanation)) = structured.get_mut("explanation") {
            if !explanation.is_empty() {
                let cut = EXPLANATION_CUTS
                    .into_iter()
                    .find(|cut| explanation.len() > cut + '…'.len_utf8());
                *explanation = match cut {
                    Some(cut) => format!("{}…", truncate_bytes(explanation, cut)),
                    None => String::new(),
                };
                self.left_out("explanation");
                return true;
            }
        }
        if let Some(Value::Array(path)) = structured.get_mut("path") {
            if !path.is_empty() {
                path.truncate(path.len() / 2);
                self.path_kept = Some(path.len());
                return true;
            }
        }
        false
    }
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
            "truncated": {
                "type": "boolean",
                "description": "Whether detail was left out to fit the server's result ceiling: \
                                then the text's headline names what (next, \
                                recommended_next, node, divergence, the explanation, or the \
                                path past its first tokens).",
            },
        },
        "required": [
            "run", "agent", "path", "verdict", "explanation", "support", "next", "truncated",
        ],
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

    #[test]
    fn check_run_guard_keeps_foresight_text_json_safe() {
        let fixture = TempStore::open("mcp-guard-foresight", crate::Config::default());
        let store = fixture.store();
        let now = crate::mcp::unix_nanos_now();
        let step = |run: &str, id: &str, tool: &str, start: u64| {
            let mut span = crate::tape::tests::span(
                &format!("{run}-trace"),
                id,
                start,
                json!({"session.id": run, "gen_ai.operation.name": "execute_tool",
                       "gen_ai.tool.name": tool}),
            );
            span.service = "svc".into();
            span
        };
        // Forty finished runs went on to a tool whose name carries a bell;
        // one live run goes somewhere new, so its foresight quotes it.
        let mut spans = Vec::new();
        for index in 0..40_u64 {
            let start = now - 3 * 3_600_000_000_000 - index * 60_000_000_000;
            let run = format!("done-{index}");
            spans.push(step(&run, "a", "plan", start));
            spans.push(step(&run, "b", "ring\u{7}bell", start + 1_000));
        }
        spans.push(step("going", "a", "plan", now - 2_000_000_000));
        spans.push(step("going", "b", "elsewhere", now - 1_000_000_000));
        store.ingest_batch(spans).expect("ingests");
        store
            .put_fuse_policy(crate::guard::PolicySpec {
                name: "svc".into(),
                matches: crate::guard::PolicyMatch {
                    service: Some("svc".into()),
                },
                limits: crate::guard::Limits {
                    min_success_odds: Some(0.5),
                    ..crate::guard::Limits::default()
                },
                ..crate::guard::PolicySpec::default()
            })
            .expect("policy");
        let server = Server::new(
            store,
            Limits {
                max_result_bytes: 16 * 1024,
                max_payload_bytes: 4096,
            },
            false,
        );
        let call = || {
            server
                .handle(
                    &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                        "params": {"name": "check_run_guard",
                                   "arguments": {"run_id": "going"}}}),
                    Context {
                        access: Access::Read,
                        tenant: None,
                        now_ns: now,
                    },
                )
                .expect("a response")
        };
        let _ = call();
        store.maintain_fate().expect("warms");
        let response = call();
        let foresight = &response["result"]["structuredContent"]["foresight"];
        assert_eq!(foresight["verdict"], "novel", "{response}");
        let explanation = foresight["explanation"].as_str().expect("explanation");
        assert!(explanation.contains("ring\u{fffd}bell"), "{explanation}");
        assert!(!explanation.chars().any(char::is_control), "{explanation}");
    }

    #[test]
    fn foresee_run_fits_every_ceiling_or_refuses_within_it() {
        let fixture = TempStore::open("mcp-foresee-ceiling", crate::Config::default());
        let store = fixture.store();
        let now = crate::mcp::unix_nanos_now();
        // Producer text at its bounds: tool names cut at 160 bytes, a path a
        // dozen of them deep, and a run id of a thousand bytes.
        let tools: Vec<String> = (0..12)
            .map(|index| format!("{index:03}{}", "t".repeat(200)))
            .collect();
        let run_of = |run: &str, start: u64, failed: bool| -> Vec<crate::Span> {
            tools
                .iter()
                .enumerate()
                .map(|(position, tool)| {
                    let mut span = crate::tape::tests::span(
                        &format!("{run}-trace"),
                        &format!("s{position}"),
                        start + position as u64 * 1_000_000_000,
                        json!({"session.id": run, "gen_ai.operation.name": "execute_tool",
                               "gen_ai.tool.name": tool}),
                    );
                    span.service = "svc".into();
                    if failed && position + 1 == tools.len() {
                        span.status = "error".into();
                    }
                    span
                })
                .collect()
        };
        let mut spans = Vec::new();
        for index in 0..40_u64 {
            spans.extend(run_of(
                &format!("done-{index}"),
                now - 3 * 3_600_000_000_000 - index * 60_000_000_000,
                index % 4 == 0,
            ));
        }
        let long_id = format!("r{}", "x".repeat(999));
        spans.extend(run_of("deep", now - 60_000_000_000, false));
        spans.extend(run_of(&long_id, now - 60_000_000_000, false));
        // A run that leaves the known paths halfway, with a long name of its
        // own: a divergence, the common next steps and what runs did next.
        let mut astray = run_of("astray", now - 60_000_000_000, false);
        astray.truncate(7);
        astray[6].attributes.insert(
            "gen_ai.tool.name".to_owned(),
            json!(format!("new{}", "n".repeat(300))),
        );
        spans.extend(astray);
        // And one whose names JSON doubles: every byte a quote.
        let mut quoted = run_of("quoted", now - 60_000_000_000, false);
        quoted.truncate(3);
        for (index, span) in quoted.iter_mut().enumerate() {
            span.attributes.insert(
                "gen_ai.tool.name".to_owned(),
                json!(format!("{index}{}", "\"".repeat(300))),
            );
        }
        spans.extend(quoted);
        store.ingest_batch(spans).expect("ingests");
        for ceiling in [super::super::MIN_RESULT_BYTES, 2_048, 32 * 1024] {
            let server = Server::new(
                store,
                Limits {
                    max_result_bytes: ceiling,
                    max_payload_bytes: 4096,
                },
                false,
            );
            for run in ["deep", "astray", "quoted", long_id.as_str()] {
                let response = server
                    .handle(
                        &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                            "params": {"name": "foresee_run", "arguments": {"run_id": run}}}),
                        Context {
                            access: Access::Read,
                            tenant: None,
                            now_ns: now,
                        },
                    )
                    .expect("a response");
                let result = &response["result"];
                let serialized = serde_json::to_vec(result).expect("serializes").len();
                let label = format!("{}-byte run id at {ceiling}", run.len());
                assert!(serialized <= ceiling, "{label}: {serialized} bytes");
                let text = result["content"][0]["text"].as_str().expect("text");
                if result["isError"] == json!(true) {
                    // Only a result whose identifiers alone overflow is
                    // refused, and the refusal quotes none of them.
                    assert!(run.len() > 100 && ceiling < 2_048, "{label}: {text}");
                    assert!(!text.contains("xxxxxxxx"), "{label}: {text}");
                    assert!(text.contains("--mcp-max-result-bytes"), "{label}: {text}");
                    continue;
                }
                let structured = &result["structuredContent"];
                for required in output_schema()["required"].as_array().unwrap() {
                    assert!(
                        structured.get(required.as_str().unwrap()).is_some(),
                        "{label}: {required} missing from {structured}"
                    );
                }
                assert_eq!(structured["run"], json!(run), "{label}");
                if run == "astray" {
                    assert_eq!(structured["verdict"], json!("novel"), "{label}");
                }
                // The headline is never what gives way.
                assert!(text.starts_with("Foresight: "), "{label}: {text}");
                if ceiling == 32 * 1024 {
                    assert_eq!(structured["truncated"], json!(false), "{label}");
                    let path = structured["path"].as_array().map(Vec::len);
                    let whole = match run {
                        "astray" => 7,
                        "quoted" => 3,
                        _ => 12,
                    };
                    assert_eq!(path, Some(whole), "{label}");
                    if run == "astray" {
                        assert!(structured["divergence"]["common_next"][0].is_string());
                        assert!(!structured["next"].as_array().unwrap().is_empty());
                    }
                } else {
                    // Cut to fit, and saying so in both halves.
                    assert_eq!(structured["truncated"], json!(true), "{label}");
                    assert!(text.contains("Truncated"), "{label}: {text}");
                }
            }
        }
    }
}
