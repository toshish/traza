//! Tapes and the replay proxy, end to end against the real binary.
//!
//! Runs are seeded the way real instrumentation records them — OTel GenAI
//! over OTLP (parts-shaped `gen_ai.input.messages`), OpenLLMetry's indexed
//! `gen_ai.prompt.{i}.*` layout for both its OpenAI and Anthropic
//! instrumentations, and OpenInference's `llm.input_messages.{i}.message.*` —
//! each including a tool call. The provider routes are then driven with raw
//! HTTP shaped exactly as the official SDKs send it: the OpenAI SDK's bearer
//! token and `/chat/completions` under its base URL, the Anthropic SDK's
//! `x-api-key` and `anthropic-version` and `/v1/messages` under its base URL.

mod common;

use common::{header, Server};
use serde_json::{json, Value};

/// 2026-01-01T00:00:00Z: old enough that every seeded run reads as finished.
const T0: u64 = 1_767_225_600_000_000_000;
const MS: u64 = 1_000_000;

const OPERATOR: &str = "operator-token";
const ACME: &str = "acme-reader-token";
const BETA: &str = "beta-reader-token";
const ACME_WRITER: &str = "acme-writer-token";

fn tokens() -> String {
    format!("admin:{OPERATOR},ro@acme:{ACME},ro@beta:{BETA},rw@acme:{ACME_WRITER}")
}

fn bearer(token: &str) -> (&'static str, String) {
    ("Authorization", format!("Bearer {token}"))
}

/// The headers the OpenAI Python SDK sends with every request.
fn openai_sdk(token: &str) -> Vec<(&'static str, String)> {
    vec![
        bearer(token),
        ("Accept", "application/json".into()),
        ("User-Agent", "OpenAI/Python 1.51.0".into()),
        ("X-Stainless-Lang", "python".into()),
        ("X-Stainless-Package-Version", "1.51.0".into()),
        ("X-Stainless-Runtime", "CPython".into()),
        ("X-Stainless-Async", "false".into()),
        ("x-stainless-retry-count", "0".into()),
    ]
}

/// The headers the Anthropic Python SDK sends: its credential is
/// `x-api-key`, never `Authorization`.
fn anthropic_sdk(token: &str) -> Vec<(&'static str, String)> {
    vec![
        ("x-api-key", token.into()),
        ("anthropic-version", "2023-06-01".into()),
        ("Accept", "application/json".into()),
        ("User-Agent", "Anthropic/Python 0.39.0".into()),
        ("X-Stainless-Lang", "python".into()),
        ("x-stainless-retry-count", "0".into()),
    ]
}

fn send(
    server: &Server,
    method: &str,
    path: &str,
    body: Option<&Value>,
    headers: &[(&'static str, String)],
) -> (u16, Vec<(String, String)>, String) {
    let borrowed: Vec<(&str, &str)> = headers
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    let encoded = body.map(|value| serde_json::to_vec(value).expect("encodes"));
    server.request(method, path, encoded.as_deref(), &borrowed)
}

fn send_json(
    server: &Server,
    method: &str,
    path: &str,
    body: Option<&Value>,
    headers: &[(&'static str, String)],
) -> (u16, Value) {
    let (status, _, text) = send(server, method, path, body, headers);
    let parsed = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("{path} answered {status} non-JSON ({error}): {text}"))
    };
    (status, parsed)
}

fn native(
    trace: &str,
    id: &str,
    parent: Option<&str>,
    name: &str,
    start_ms: u64,
    end_ms: u64,
    attributes: Value,
) -> Value {
    json!({
        "trace_id": trace, "span_id": id, "parent_span_id": parent, "name": name,
        "service": "agent", "status": "ok",
        "start_time_ns": T0 + start_ms * MS, "end_time_ns": T0 + end_ms * MS,
        "attributes": attributes,
    })
}

/// Adds `key: value` to every span's attributes.
fn with(mut spans: Vec<Value>, key: &str, value: &str) -> Vec<Value> {
    for span in &mut spans {
        span["attributes"][key] = json!(value);
    }
    spans
}

fn ingest(server: &Server, spans: Vec<Value>, token: Option<&str>) {
    let headers: Vec<(&'static str, String)> = token.map(bearer).into_iter().collect();
    let (status, body) = send_json(server, "POST", "/v1/spans", Some(&json!(spans)), &headers);
    assert_eq!(status, 200, "{body}");
}

// ---------------------------------------------------------------- the runs

/// A weather agent instrumented with OpenLLMetry's OpenAI instrumentation:
/// a workflow wrapper, a model call that asks for a tool, the tool, and the
/// model call that answers.
fn openllmetry_run() -> Vec<Value> {
    let system = "You are a weather assistant.";
    let user = "What's the weather in Paris?";
    with(
        vec![
            native(
                "tr-llmetry",
                "wf",
                None,
                "weather_agent.workflow",
                0,
                1000,
                json!({
                    "traceloop.span.kind": "workflow", "traceloop.entity.name": "weather_agent",
                }),
            ),
            native(
                "tr-llmetry",
                "llm-1",
                Some("wf"),
                "openai.chat",
                10,
                400,
                json!({
                    "gen_ai.system": "openai", "llm.request.type": "chat",
                    "gen_ai.request.model": "gpt-4o",
                    "gen_ai.response.model": "gpt-4o-2024-08-06",
                    "gen_ai.response.id": "chatcmpl-AbC123",
                    "gen_ai.usage.prompt_tokens": 82, "gen_ai.usage.completion_tokens": 17,
                    "llm.usage.total_tokens": 99,
                    "gen_ai.prompt.0.role": "system", "gen_ai.prompt.0.content": system,
                    "gen_ai.prompt.1.role": "user", "gen_ai.prompt.1.content": user,
                    "gen_ai.completion.0.role": "assistant",
                    "gen_ai.completion.0.finish_reason": "tool_calls",
                    "gen_ai.completion.0.tool_calls.0.id": "call_w1",
                    "gen_ai.completion.0.tool_calls.0.name": "get_weather",
                    "gen_ai.completion.0.tool_calls.0.arguments": "{\"city\": \"Paris\"}",
                }),
            ),
            native(
                "tr-llmetry",
                "tool-1",
                Some("wf"),
                "get_weather.tool",
                410,
                450,
                json!({
                    "traceloop.span.kind": "tool", "traceloop.entity.name": "get_weather",
                    "traceloop.entity.input": "{\"city\": \"Paris\"}",
                    "traceloop.entity.output": "{\"forecast\": \"rain\", \"celsius\": 12}",
                }),
            ),
            native(
                "tr-llmetry",
                "llm-2",
                Some("wf"),
                "openai.chat",
                460,
                900,
                json!({
                    "gen_ai.system": "openai", "llm.request.type": "chat",
                    "gen_ai.request.model": "gpt-4o",
                    "gen_ai.response.model": "gpt-4o-2024-08-06",
                    "gen_ai.usage.prompt_tokens": 120, "gen_ai.usage.completion_tokens": 14,
                    "gen_ai.prompt.0.role": "system", "gen_ai.prompt.0.content": system,
                    "gen_ai.prompt.1.role": "user", "gen_ai.prompt.1.content": user,
                    "gen_ai.prompt.2.role": "assistant",
                    "gen_ai.prompt.2.tool_calls.0.id": "call_w1",
                    "gen_ai.prompt.2.tool_calls.0.name": "get_weather",
                    "gen_ai.prompt.2.tool_calls.0.arguments": "{\"city\": \"Paris\"}",
                    "gen_ai.prompt.3.role": "tool", "gen_ai.prompt.3.tool_call_id": "call_w1",
                    "gen_ai.prompt.3.content": "{\"forecast\": \"rain\", \"celsius\": 12}",
                    "gen_ai.completion.0.role": "assistant",
                    "gen_ai.completion.0.finish_reason": "stop",
                    "gen_ai.completion.0.content": "It's raining in Paris, 12°C. Take an umbrella.",
                }),
            ),
        ],
        "traceloop.association.properties.session_id",
        "sess-llmetry",
    )
}

/// The two requests the weather agent's OpenAI SDK sends, as the SDK sends
/// them. The second carries the first answer's tool call back exactly as the
/// SDK serializes a replayed `tool_calls` entry.
fn weather_requests(stream: bool) -> (Value, Value) {
    let tools = json!([{"type": "function", "function": {
        "name": "get_weather", "parameters": {"type": "object",
            "properties": {"city": {"type": "string"}}}}}]);
    let base = json!([
        {"role": "system", "content": "You are a weather assistant."},
        {"role": "user", "content": "What's the weather in Paris?"},
    ]);
    let mut first = json!({"model": "gpt-4o", "messages": base.clone(), "tools": tools.clone()});
    let mut messages = base.as_array().unwrap().clone();
    messages.push(
        json!({"role": "assistant", "content": null, "tool_calls": [{
        "id": "call_w1", "type": "function",
        "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}]}),
    );
    messages.push(json!({"role": "tool", "tool_call_id": "call_w1",
                         "content": "{\"forecast\": \"rain\", \"celsius\": 12}"}));
    let mut second = json!({"model": "gpt-4o", "messages": messages, "tools": tools});
    if stream {
        for request in [&mut first, &mut second] {
            request["stream"] = json!(true);
            request["stream_options"] = json!({"include_usage": true});
        }
    }
    (first, second)
}

/// An order agent instrumented with OpenInference: an agent span around two
/// model calls and a tool.
fn openinference_run() -> Vec<Value> {
    let model = json!({"llm.model_name": "gpt-4o-mini", "llm.provider": "openai"});
    let mut first = json!({
        "openinference.span.kind": "LLM",
        "llm.token_count.prompt": 51, "llm.token_count.completion": 22,
        "llm.input_messages.0.message.role": "system",
        "llm.input_messages.0.message.content": "You track orders.",
        "llm.input_messages.1.message.role": "user",
        "llm.input_messages.1.message.content": "Where is order A-17?",
        "llm.output_messages.0.message.role": "assistant",
        "llm.output_messages.0.message.tool_calls.0.tool_call.id": "call_oi_1",
        "llm.output_messages.0.message.tool_calls.0.tool_call.function.name": "lookup_order",
        "llm.output_messages.0.message.tool_calls.0.tool_call.function.arguments":
            "{\"order_id\": \"A-17\"}",
    });
    let mut second = json!({
        "openinference.span.kind": "LLM",
        "llm.token_count.prompt": 90, "llm.token_count.completion": 11,
        "llm.input_messages.0.message.role": "system",
        "llm.input_messages.0.message.content": "You track orders.",
        "llm.input_messages.1.message.role": "user",
        "llm.input_messages.1.message.content": "Where is order A-17?",
        "llm.input_messages.2.message.role": "assistant",
        "llm.input_messages.2.message.tool_calls.0.tool_call.id": "call_oi_1",
        "llm.input_messages.2.message.tool_calls.0.tool_call.function.name": "lookup_order",
        "llm.input_messages.2.message.tool_calls.0.tool_call.function.arguments":
            "{\"order_id\": \"A-17\"}",
        "llm.input_messages.3.message.role": "tool",
        "llm.input_messages.3.message.tool_call_id": "call_oi_1",
        "llm.input_messages.3.message.content": "shipped",
        "llm.output_messages.0.message.role": "assistant",
        "llm.output_messages.0.message.content": "Order A-17 has shipped.",
    });
    for span in [&mut first, &mut second] {
        for (key, value) in model.as_object().unwrap() {
            span[key] = value.clone();
        }
    }
    with(
        vec![
            native(
                "tr-oi",
                "agent",
                None,
                "OrderAgent",
                0,
                900,
                json!({"openinference.span.kind": "AGENT", "agent.name": "order-agent"}),
            ),
            native(
                "tr-oi",
                "llm-1",
                Some("agent"),
                "ChatCompletion",
                10,
                300,
                first,
            ),
            native(
                "tr-oi",
                "tool-1",
                Some("agent"),
                "lookup_order",
                310,
                330,
                json!({
                    "openinference.span.kind": "TOOL", "tool.name": "lookup_order",
                    "input.value": "{\"order_id\": \"A-17\"}", "output.value": "shipped",
                }),
            ),
            native(
                "tr-oi",
                "llm-2",
                Some("agent"),
                "ChatCompletion",
                340,
                800,
                second,
            ),
        ],
        "session.id",
        "sess-oi",
    )
}

/// A travel agent instrumented with the current OTel GenAI conventions and
/// exported over OTLP/JSON: message lists are JSON strings in `parts` shape,
/// the system prompt is `gen_ai.system_instructions`, and finish reasons are
/// an array attribute.
fn otel_genai_otlp() -> Value {
    let text = |value: &str| json!({"stringValue": value});
    let int = |value: u64| json!({"intValue": value.to_string()});
    let attr = |key: &str, value: Value| json!({"key": key, "value": value});
    let json_attr = |key: &str, value: Value| json!({"key": key, "value": {"stringValue": serde_json::to_string(&value).unwrap()}});
    let system = json!([{"type": "text", "content": "You plan trips."}]);
    let user = json!({"role": "user", "parts": [
        {"type": "text", "content": "Find me a flight to Lisbon on Friday."}]});
    let call = json!({"role": "assistant", "parts": [{"type": "tool_call", "id": "call_otel_1",
        "name": "search_flights", "arguments": {"to": "LIS", "day": "friday"}}],
        "finish_reason": "tool_call"});
    let result = json!({"role": "tool", "parts": [{"type": "tool_call_response",
        "id": "call_otel_1", "response": "TP1350 departs 09:05"}]});
    let answer = json!([{"role": "assistant", "parts": [
        {"type": "text", "content": "TP1350 leaves Friday at 09:05."}],
        "finish_reason": "stop"}]);
    let conversation = attr("gen_ai.conversation.id", text("conv-otel"));
    let span = |id: &str,
                parent: Option<&str>,
                name: &str,
                start: u64,
                end: u64,
                attributes: Vec<Value>| {
        let mut span = json!({
            "traceId": "0af7651916cd43dd8448eb211c80319c", "spanId": id, "name": name,
            "startTimeUnixNano": (T0 + start * MS).to_string(),
            "endTimeUnixNano": (T0 + end * MS).to_string(),
            "attributes": attributes,
        });
        if let Some(parent) = parent {
            span["parentSpanId"] = json!(parent);
        }
        span
    };
    json!({"resourceSpans": [{
        "resource": {"attributes": [attr("service.name", text("agent"))]},
        "scopeSpans": [{"spans": [
            span("b7ad6b7169203331", None, "invoke_agent travel-planner", 0, 1000, vec![
                attr("gen_ai.operation.name", text("invoke_agent")),
                attr("gen_ai.agent.name", text("travel-planner")),
                conversation.clone(),
            ]),
            span("00f067aa0ba902b7", Some("b7ad6b7169203331"), "chat gpt-4.1", 10, 500, vec![
                attr("gen_ai.operation.name", text("chat")),
                attr("gen_ai.provider.name", text("openai")),
                attr("gen_ai.request.model", text("gpt-4.1")),
                attr("gen_ai.response.model", text("gpt-4.1-2025-04-14")),
                attr("gen_ai.usage.input_tokens", int(140)),
                attr("gen_ai.usage.output_tokens", int(31)),
                attr("gen_ai.response.finish_reasons",
                     json!({"arrayValue": {"values": [text("tool_calls")]}})),
                json_attr("gen_ai.system_instructions", system.clone()),
                json_attr("gen_ai.input.messages", json!([user.clone()])),
                json_attr("gen_ai.output.messages", json!([call.clone()])),
                conversation.clone(),
            ]),
            span("53995c3f42cd8ad8", Some("b7ad6b7169203331"), "execute_tool search_flights",
                 510, 600, vec![
                attr("gen_ai.operation.name", text("execute_tool")),
                attr("gen_ai.tool.name", text("search_flights")),
                attr("gen_ai.tool.call.id", text("call_otel_1")),
                json_attr("gen_ai.tool.call.arguments", json!({"to": "LIS", "day": "friday"})),
                attr("gen_ai.tool.call.result", text("TP1350 departs 09:05")),
                conversation.clone(),
            ]),
            span("1d7e8c2f7a6b5c4d", Some("b7ad6b7169203331"), "chat gpt-4.1", 610, 950, vec![
                attr("gen_ai.operation.name", text("chat")),
                attr("gen_ai.provider.name", text("openai")),
                attr("gen_ai.request.model", text("gpt-4.1")),
                attr("gen_ai.usage.input_tokens", int(201)),
                attr("gen_ai.usage.output_tokens", int(12)),
                json_attr("gen_ai.system_instructions", system),
                json_attr("gen_ai.input.messages", json!([user, call, result])),
                json_attr("gen_ai.output.messages", answer),
                conversation,
            ]),
        ]}],
    }]})
}

/// A support agent instrumented with OpenLLMetry's Anthropic
/// instrumentation: the system prompt is recorded as message 0 and content
/// block lists are JSON-encoded, exactly as it records them.
fn anthropic_run() -> Vec<Value> {
    let blocks = json!([
        {"type": "text", "text": "Let me check that order."},
        {"type": "tool_use", "id": "toolu_01A", "name": "lookup_order",
         "input": {"order_id": "A-17"}},
    ]);
    let result = json!([{"type": "tool_result", "tool_use_id": "toolu_01A", "content": "shipped"}]);
    let common = json!({
        "gen_ai.system": "anthropic", "llm.request.type": "chat",
        "gen_ai.request.model": "claude-sonnet-4-5",
        "gen_ai.response.model": "claude-sonnet-4-5-20250929",
        "gen_ai.prompt.0.role": "system", "gen_ai.prompt.0.content": "You are a support agent.",
        "gen_ai.prompt.1.role": "user", "gen_ai.prompt.1.content": "Where is order A-17?",
    });
    let mut first = common.clone();
    for (key, value) in json!({
        "gen_ai.response.id": "msg_01XFDUDYJgAACzvnptvVoYEL",
        "gen_ai.usage.prompt_tokens": 60, "gen_ai.usage.completion_tokens": 40,
        "gen_ai.usage.cache_read_input_tokens": 1024,
        "gen_ai.completion.0.role": "assistant",
        "gen_ai.completion.0.finish_reason": "tool_use",
        "gen_ai.completion.0.content": "Let me check that order.",
        "gen_ai.completion.0.tool_calls.0.id": "toolu_01A",
        "gen_ai.completion.0.tool_calls.0.name": "lookup_order",
        "gen_ai.completion.0.tool_calls.0.arguments": "{\"order_id\": \"A-17\"}",
    })
    .as_object()
    .unwrap()
    {
        first[key] = value.clone();
    }
    let mut second = common;
    for (key, value) in json!({
        "gen_ai.usage.prompt_tokens": 95, "gen_ai.usage.completion_tokens": 9,
        "gen_ai.prompt.2.role": "assistant",
        "gen_ai.prompt.2.content": serde_json::to_string(&blocks).unwrap(),
        "gen_ai.prompt.3.role": "user",
        "gen_ai.prompt.3.content": serde_json::to_string(&result).unwrap(),
        "gen_ai.completion.0.role": "assistant",
        "gen_ai.completion.0.finish_reason": "end_turn",
        "gen_ai.completion.0.content": "Order A-17 has shipped and arrives Thursday.",
    })
    .as_object()
    .unwrap()
    {
        second[key] = value.clone();
    }
    with(
        vec![
            native("tr-claude", "llm-1", None, "anthropic.chat", 0, 300, first),
            native(
                "tr-claude",
                "tool-1",
                None,
                "lookup_order.tool",
                310,
                330,
                json!({
                    "traceloop.span.kind": "tool", "traceloop.entity.name": "lookup_order",
                    "traceloop.entity.input": "{\"order_id\": \"A-17\"}",
                    "traceloop.entity.output": "shipped",
                }),
            ),
            native(
                "tr-claude",
                "llm-2",
                None,
                "anthropic.chat",
                340,
                700,
                second,
            ),
        ],
        "traceloop.association.properties.session_id",
        "sess-claude",
    )
}

/// The two requests the support agent's Anthropic SDK sends.
fn support_requests(stream: bool) -> (Value, Value) {
    let tools = json!([{"name": "lookup_order", "description": "Look up an order",
        "input_schema": {"type": "object", "properties": {"order_id": {"type": "string"}}}}]);
    let first = json!({"model": "claude-sonnet-4-5", "max_tokens": 1024,
        "system": "You are a support agent.", "tools": tools,
        "messages": [{"role": "user", "content": "Where is order A-17?"}]});
    let mut second = json!({"model": "claude-sonnet-4-5", "max_tokens": 1024,
    "system": "You are a support agent.", "tools": tools,
    "messages": [
        {"role": "user", "content": "Where is order A-17?"},
        {"role": "assistant", "content": [
            {"type": "text", "text": "Let me check that order."},
            {"type": "tool_use", "id": "toolu_01A", "name": "lookup_order",
             "input": {"order_id": "A-17"}}]},
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_01A", "content": "shipped"}]},
    ]});
    if stream {
        second["stream"] = json!(true);
    }
    (first, second)
}

fn seeded_open_server(args: &[&str]) -> Server {
    let server = Server::spawn(args);
    ingest(&server, openllmetry_run(), None);
    ingest(&server, openinference_run(), None);
    ingest(&server, anthropic_run(), None);
    let (status, body) = send_json(&server, "POST", "/v1/traces", Some(&otel_genai_otlp()), &[]);
    assert_eq!(status, 200, "{body}");
    server
}

fn create_replay(server: &Server, body: Value, headers: &[(&'static str, String)]) -> Value {
    let (status, created) = send_json(server, "POST", "/v1/replays", Some(&body), headers);
    assert_eq!(status, 201, "{created}");
    created
}

/// Parses a `text/event-stream` body into `(event name, data)` pairs.
fn sse(body: &str) -> Vec<(Option<String>, String)> {
    body.split("\n\n")
        .filter(|block| !block.trim().is_empty())
        .map(|block| {
            let mut name = None;
            let mut data = String::new();
            for line in block.lines() {
                if let Some(value) = line.strip_prefix("event: ") {
                    name = Some(value.to_owned());
                } else if let Some(value) = line.strip_prefix("data: ") {
                    data.push_str(value);
                }
            }
            (name, data)
        })
        .collect()
}

// ------------------------------------------------------------------- tapes

#[test]
fn tapes_put_every_conventions_steps_in_order_with_their_facts() {
    let server = seeded_open_server(&[]);

    // OpenLLMetry: the workflow wrapper is context, not a step.
    let (status, tape) = send_json(&server, "GET", "/v1/tapes/sess-llmetry", None, &[]);
    assert_eq!(status, 200, "{tape}");
    assert_eq!(tape["run"], json!("sess-llmetry"));
    assert_eq!(
        tape["tenant"],
        Value::Null,
        "the operator view names no tenant"
    );
    assert_eq!(tape["spans_examined"], json!(4));
    assert_eq!(tape["truncated"], json!(false));
    assert!(tape.get("resolved_partial").is_none());
    let steps = tape["steps"].as_array().expect("steps");
    let kinds: Vec<&str> = steps
        .iter()
        .map(|step| step["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["llm", "tool", "llm"]);
    let indexes: Vec<u64> = steps
        .iter()
        .map(|step| step["index"].as_u64().unwrap())
        .collect();
    assert_eq!(indexes, [0, 1, 2]);
    let first = &steps[0];
    assert_eq!(first["span_id"], json!("llm-1"));
    assert_eq!(first["parent_span_id"], json!("wf"));
    assert_eq!(first["model"], json!("gpt-4o-2024-08-06"));
    assert_eq!(first["start_ns"], json!(T0 + 10 * MS));
    assert_eq!(first["duration_ns"], json!(390 * MS));
    assert_eq!(first["status"], json!("ok"));
    assert_eq!(
        first["tokens"],
        json!({"input": 82, "output": 17, "cache_read": null, "cache_write": null, "context": 82})
    );
    assert_eq!(
        first["request"],
        json!([
            {"role": "system", "content": "You are a weather assistant."},
            {"role": "user", "content": "What's the weather in Paris?"},
        ])
    );
    assert_eq!(
        first["response"][0]["tool_calls"][0],
        json!({"id": "call_w1", "name": "get_weather", "arguments": {"city": "Paris"}})
    );
    for step in steps {
        let digest = step["request_digest"]
            .as_str()
            .expect("every step has input");
        assert_eq!(digest.len(), 64);
        assert!(step["response_digest"].as_str().is_some());
        let fingerprint = step["fingerprint"].as_str().unwrap();
        assert!(
            fingerprint.ends_with(&format!("#{}", &digest[..16])),
            "{fingerprint}"
        );
    }
    assert!(steps[0]["fingerprint"]
        .as_str()
        .unwrap()
        .starts_with("llm:gpt-4o-2024-08-06#"));
    assert_eq!(steps[1]["tool"], json!("get_weather"));
    assert_eq!(steps[1]["request"], json!({"city": "Paris"}));
    assert!(steps[1].get("model").is_none(), "absent facts are omitted");
    assert_ne!(steps[0]["request_digest"], steps[2]["request_digest"]);

    // OpenInference: the agent span IS a step.
    let (status, tape) = send_json(&server, "GET", "/v1/tapes/sess-oi", None, &[]);
    assert_eq!(status, 200, "{tape}");
    let steps = tape["steps"].as_array().unwrap();
    let kinds: Vec<&str> = steps
        .iter()
        .map(|step| step["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["agent", "llm", "tool", "llm"]);
    assert_eq!(steps[0]["agent"], json!("order-agent"));
    assert_eq!(steps[1]["model"], json!("gpt-4o-mini"));
    assert_eq!(steps[1]["tokens"]["input"], json!(51));
    assert_eq!(steps[2]["tool"], json!("lookup_order"));
    assert_eq!(steps[2]["response"], json!("shipped"));
    assert_eq!(
        steps[3]["request"][3],
        json!({"role": "tool", "tool_call_id": "call_oi_1", "content": "shipped"})
    );

    // OTel GenAI over OTLP: the conversation id is the run; parts survive;
    // the system instructions lead the request.
    let (status, tape) = send_json(&server, "GET", "/v1/tapes/conv-otel", None, &[]);
    assert_eq!(status, 200, "{tape}");
    let steps = tape["steps"].as_array().unwrap();
    let kinds: Vec<&str> = steps
        .iter()
        .map(|step| step["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["agent", "llm", "tool", "llm"]);
    assert_eq!(steps[0]["agent"], json!("travel-planner"));
    assert_eq!(steps[1]["model"], json!("gpt-4.1-2025-04-14"));
    assert_eq!(steps[1]["request"][0]["role"], json!("system"));
    assert_eq!(steps[1]["request"][1]["parts"][0]["type"], json!("text"));
    assert_eq!(steps[2]["tool"], json!("search_flights"));
    assert_eq!(steps[2]["request"], json!({"to": "LIS", "day": "friday"}));

    // A trace id names its run too.
    let (status, tape) = send_json(&server, "GET", "/v1/tapes/trace:tr-claude", None, &[]);
    assert_eq!(status, 200, "{tape}");
    assert_eq!(tape["steps"].as_array().unwrap().len(), 3);

    // max_steps truncates and says so; unknown runs and parameters refuse.
    let (_, capped) = send_json(&server, "GET", "/v1/tapes/sess-oi?max_steps=2", None, &[]);
    assert_eq!(capped["steps"].as_array().unwrap().len(), 2);
    assert_eq!(capped["truncated"], json!(true));
    let (status, body) = send_json(&server, "GET", "/v1/tapes/no-such-run", None, &[]);
    assert_eq!((status, body), (404, json!({"error": "run not found"})));
    let (status, body) = send_json(&server, "GET", "/v1/tapes/sess-oi?bogus=1", None, &[]);
    assert_eq!(status, 400, "{body}");
    let (status, _) = send_json(&server, "GET", "/v1/tapes/sess-oi?resolve=maybe", None, &[]);
    assert_eq!(status, 400);
}

#[test]
fn resolve_inlines_offloaded_payloads_within_the_tape_budget() {
    // A tiny offload threshold makes the long prompts below payload files.
    let server = Server::spawn(&["--payload-threshold-bytes", "64", "--tape-max-bytes", "300"]);
    let long = |letter: &str| letter.repeat(200);
    ingest(
        &server,
        vec![
            native(
                "tr-big",
                "one",
                None,
                "chat",
                0,
                10,
                json!({
                    "gen_ai.operation.name": "chat", "gen_ai.request.model": "m",
                    "gen_ai.prompt.0.role": "user", "gen_ai.prompt.0.content": long("a"),
                    "gen_ai.completion.0.content": "ok",
                }),
            ),
            native(
                "tr-big",
                "two",
                None,
                "chat",
                20,
                30,
                json!({
                    "gen_ai.operation.name": "chat", "gen_ai.request.model": "m",
                    "gen_ai.prompt.0.role": "user", "gen_ai.prompt.0.content": long("b"),
                    "gen_ai.completion.0.content": "ok",
                }),
            ),
        ],
        None,
    );
    let (_, stored) = send_json(&server, "GET", "/v1/tapes/trace:tr-big", None, &[]);
    let reference = &stored["steps"][0]["request"][0]["content"];
    assert!(
        reference["$payload"]
            .as_str()
            .is_some_and(|r| r.starts_with("sha256/")),
        "{stored}"
    );

    let (status, resolved) = send_json(
        &server,
        "GET",
        "/v1/tapes/trace:tr-big?resolve=true",
        None,
        &[],
    );
    assert_eq!(status, 200, "{resolved}");
    // 300 bytes fit the first 200-byte payload and not the second.
    assert_eq!(resolved["resolved_partial"], json!(true));
    assert_eq!(
        resolved["steps"][0]["request"][0]["content"],
        json!(long("a"))
    );
    assert!(resolved["steps"][1]["request"][0]["content"]["$payload"].is_string());
    // Identities describe the stored form either way.
    assert_eq!(
        stored["steps"][0]["request_digest"],
        resolved["steps"][0]["request_digest"]
    );
}

// ----------------------------------------------------------------- replays

#[test]
fn the_openai_sdk_replays_a_recorded_run_with_zero_model_calls() {
    let server = Server::spawn_with_env(&[], &[("TRAZA_TOKENS", &tokens())]);
    ingest(&server, openllmetry_run(), Some(OPERATOR));

    // Creating a replay is a read of the store; the tenancy test below shows
    // a read-only credential doing it.
    let created = create_replay(&server, json!({"run": "sess-llmetry"}), &[bearer(OPERATOR)]);
    let id = created["id"].as_str().expect("id");
    assert!(id.starts_with("rp_"), "{created}");
    assert_eq!(created["run"], json!("sess-llmetry"));
    assert_eq!(created["mode"], json!("strict"), "strict is the default");
    assert_eq!(
        (created["steps"].as_u64(), created["llm_steps"].as_u64()),
        (Some(3), Some(2))
    );
    let base = created["base_urls"]["openai"].as_str().unwrap().to_owned();
    assert_eq!(base, format!("/v1/replays/{id}/openai/v1"));
    assert_eq!(
        created["base_urls"]["anthropic"],
        json!(format!("/v1/replays/{id}/anthropic"))
    );
    assert!(created["expires_at_ns"].as_u64().unwrap() > T0);

    let (first, second) = weather_requests(false);
    let path = format!("{base}/chat/completions");
    let (status, headers, text) = send(&server, "POST", &path, Some(&first), &openai_sdk(OPERATOR));
    assert_eq!(status, 200, "{text}");
    assert_eq!(header(&headers, "content-type"), Some("application/json"));
    let completion: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(completion["object"], json!("chat.completion"));
    assert_eq!(
        completion["id"],
        json!("chatcmpl-AbC123"),
        "the recorded response id"
    );
    assert_eq!(completion["model"], json!("gpt-4o-2024-08-06"));
    assert_eq!(completion["created"], json!((T0 + 10 * MS) / 1_000_000_000));
    let choice = &completion["choices"][0];
    assert_eq!(choice["index"], json!(0));
    assert_eq!(choice["finish_reason"], json!("tool_calls"));
    assert_eq!(choice["message"]["role"], json!("assistant"));
    assert_eq!(choice["message"]["content"], Value::Null);
    assert_eq!(
        choice["message"]["tool_calls"],
        json!([{"id": "call_w1", "type": "function",
                "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}])
    );
    assert_eq!(
        completion["usage"],
        json!({"prompt_tokens": 82, "completion_tokens": 17, "total_tokens": 99})
    );

    let (status, completion) =
        send_json(&server, "POST", &path, Some(&second), &openai_sdk(OPERATOR));
    assert_eq!(status, 200, "{completion}");
    assert_eq!(
        completion["choices"][0]["message"]["content"],
        json!("It's raining in Paris, 12°C. Take an umbrella.")
    );
    assert_eq!(completion["choices"][0]["finish_reason"], json!("stop"));
    assert!(completion["choices"][0]["message"]
        .get("tool_calls")
        .is_none());
    assert!(completion["id"].as_str().unwrap().starts_with("chatcmpl-"));

    // Past the end: 410, in the provider's error shape, not to be retried.
    let (status, headers, text) =
        send(&server, "POST", &path, Some(&second), &openai_sdk(OPERATOR));
    assert_eq!(status, 410, "{text}");
    assert_eq!(header(&headers, "x-should-retry"), Some("false"));
    let error: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(error["error"]["type"], json!("replay_exhausted"));
    assert_eq!(error["error"]["served"], json!(2));
    assert!(error["error"]["message"].as_str().unwrap().contains('2'));

    let (status, report) = send_json(
        &server,
        "GET",
        &format!("/v1/replays/{id}"),
        None,
        &[bearer(OPERATOR)],
    );
    assert_eq!(status, 200, "{report}");
    assert_eq!(report["served"], json!(2));
    assert_eq!(report["remaining"], json!(0));
    assert_eq!(report["mode"], json!("strict"));
    assert_eq!(report["run"], json!("sess-llmetry"));
    assert_eq!(report["diverged"], Value::Null);
    let calls = report["calls"].as_array().unwrap();
    assert_eq!(calls.len(), 2, "exhaustion is not a served call: {report}");
    assert_eq!(calls[0]["step"], json!(0));
    assert_eq!(calls[1]["step"], json!(2));
    assert!(calls.iter().all(|call| call["matched"] == json!(true)));
    assert!(calls[0]["at_ns"].as_u64().unwrap() > T0);
}

#[test]
fn the_openai_sdk_streams_reconstructed_chunks() {
    let server = seeded_open_server(&[]);
    let created = create_replay(&server, json!({"run": "sess-llmetry"}), &[]);
    let path = format!(
        "{}/chat/completions",
        created["base_urls"]["openai"].as_str().unwrap()
    );
    let (first, second) = weather_requests(true);

    let (status, headers, body) = send(&server, "POST", &path, Some(&first), &openai_sdk("unused"));
    assert_eq!(status, 200, "{body}");
    assert_eq!(header(&headers, "content-type"), Some("text/event-stream"));
    let events = sse(&body);
    assert_eq!(events.last().map(|(_, data)| data.as_str()), Some("[DONE]"));
    let mut arguments = String::new();
    let mut finish = None;
    let mut usage = None;
    for (_, data) in &events[..events.len() - 1] {
        let chunk: Value = serde_json::from_str(data).unwrap();
        assert_eq!(chunk["object"], json!("chat.completion.chunk"));
        assert_eq!(chunk["id"], json!("chatcmpl-AbC123"));
        match chunk["choices"].get(0) {
            Some(choice) => {
                if let Some(call) = choice["delta"]["tool_calls"].get(0) {
                    assert_eq!(call["index"], json!(0));
                    if let Some(name) = call["function"]["name"].as_str() {
                        assert_eq!(
                            (name, call["id"].as_str()),
                            ("get_weather", Some("call_w1"))
                        );
                    }
                    arguments.push_str(call["function"]["arguments"].as_str().unwrap_or_default());
                }
                if let Some(reason) = choice["finish_reason"].as_str() {
                    finish = Some(reason.to_owned());
                }
            }
            None => usage = Some(chunk["usage"].clone()),
        }
    }
    assert_eq!(
        serde_json::from_str::<Value>(&arguments).unwrap(),
        json!({"city": "Paris"})
    );
    assert_eq!(finish.as_deref(), Some("tool_calls"));
    assert_eq!(
        usage.expect("include_usage sends a usage chunk")["total_tokens"],
        json!(99)
    );

    let (status, _, body) = send(&server, "POST", &path, Some(&second), &openai_sdk("unused"));
    assert_eq!(status, 200, "{body}");
    let mut text = String::new();
    let events = sse(&body);
    let opening: Value = serde_json::from_str(&events[0].1).unwrap();
    assert_eq!(opening["choices"][0]["delta"]["role"], json!("assistant"));
    for (_, data) in &events[..events.len() - 1] {
        let chunk: Value = serde_json::from_str(data).unwrap();
        if let Some(piece) = chunk["choices"][0]["delta"]["content"].as_str() {
            text.push_str(piece);
        }
    }
    assert_eq!(text, "It's raining in Paris, 12°C. Take an umbrella.");
    assert!(events.len() > 4, "the text arrives in several deltas");
}

#[test]
fn the_anthropic_sdk_replays_with_its_own_credential_header() {
    let server = Server::spawn_with_env(&[], &[("TRAZA_TOKENS", &tokens())]);
    ingest(&server, anthropic_run(), Some(OPERATOR));
    let created = create_replay(&server, json!({"run": "sess-claude"}), &[bearer(OPERATOR)]);
    let base = created["base_urls"]["anthropic"].as_str().unwrap();
    let path = format!("{base}/v1/messages");
    let (first, second) = support_requests(true);

    // No credential at all is refused before anything is served.
    let (status, _, _) = send(&server, "POST", &path, Some(&first), &[]);
    assert_eq!(status, 401);

    let (status, headers, text) = send(
        &server,
        "POST",
        &path,
        Some(&first),
        &anthropic_sdk(OPERATOR),
    );
    assert_eq!(status, 200, "{text}");
    assert_eq!(header(&headers, "content-type"), Some("application/json"));
    let message: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(message["type"], json!("message"));
    assert_eq!(message["role"], json!("assistant"));
    assert_eq!(message["id"], json!("msg_01XFDUDYJgAACzvnptvVoYEL"));
    assert_eq!(message["model"], json!("claude-sonnet-4-5-20250929"));
    assert_eq!(
        message["content"],
        json!([
            {"type": "text", "text": "Let me check that order."},
            {"type": "tool_use", "id": "toolu_01A", "name": "lookup_order",
             "input": {"order_id": "A-17"}},
        ])
    );
    assert_eq!(message["stop_reason"], json!("tool_use"));
    assert_eq!(message["stop_sequence"], Value::Null);
    assert_eq!(
        message["usage"],
        json!({"input_tokens": 60, "output_tokens": 40, "cache_read_input_tokens": 1024})
    );

    let (status, headers, body) = send(
        &server,
        "POST",
        &path,
        Some(&second),
        &anthropic_sdk(OPERATOR),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(header(&headers, "content-type"), Some("text/event-stream"));
    let events = sse(&body);
    let names: Vec<&str> = events
        .iter()
        .map(|(name, _)| name.as_deref().unwrap())
        .collect();
    assert_eq!(names[0], "message_start");
    assert_eq!(names[1], "content_block_start");
    assert_eq!(
        &names[names.len() - 3..],
        ["content_block_stop", "message_delta", "message_stop"]
    );
    let mut text = String::new();
    for (name, data) in &events {
        let event: Value = serde_json::from_str(data).unwrap();
        assert_eq!(
            event["type"],
            json!(name.as_deref().unwrap()),
            "event and type agree"
        );
        match name.as_deref() {
            Some("message_start") => {
                assert_eq!(event["message"]["content"], json!([]));
                assert_eq!(event["message"]["usage"]["input_tokens"], json!(95));
            }
            Some("content_block_delta") => {
                assert_eq!(event["delta"]["type"], json!("text_delta"));
                text.push_str(event["delta"]["text"].as_str().unwrap());
            }
            Some("message_delta") => {
                assert_eq!(event["delta"]["stop_reason"], json!("end_turn"));
                assert_eq!(event["usage"]["output_tokens"], json!(9));
            }
            _ => {}
        }
    }
    assert_eq!(text, "Order A-17 has shipped and arrives Thursday.");

    // x-api-key is a credential on replay call routes ONLY.
    let (status, _, _) = send(
        &server,
        "GET",
        "/v1/spans",
        None,
        &[("x-api-key", OPERATOR.into())],
    );
    assert_eq!(status, 401, "x-api-key must not authenticate elsewhere");
    let (status, _, _) = send(
        &server,
        "POST",
        "/v1/replays",
        Some(&json!({"run": "sess-claude"})),
        &[("x-api-key", OPERATOR.into())],
    );
    assert_eq!(status, 401, "nor replay creation");
    let (status, _, _) = send(
        &server,
        "POST",
        &path,
        Some(&first),
        &anthropic_sdk("wrong-token"),
    );
    assert_eq!(status, 401);
}

#[test]
fn strict_divergence_is_refused_without_consuming_the_step() {
    let server = seeded_open_server(&[]);
    let created = create_replay(
        &server,
        json!({"run": "sess-llmetry", "mode": "strict"}),
        &[],
    );
    let id = created["id"].as_str().unwrap();
    let path = format!("/v1/replays/{id}/openai/v1/chat/completions");
    let (first, _) = weather_requests(false);

    // The agent asks about a different city: a different question.
    let mut changed = first.clone();
    changed["messages"][1]["content"] = json!("What's the weather in Rome?");
    let (status, headers, text) = send(&server, "POST", &path, Some(&changed), &openai_sdk("x"));
    assert_eq!(status, 409, "{text}");
    // Both SDKs retry a 409 unless told not to.
    assert_eq!(header(&headers, "x-should-retry"), Some("false"));
    let error: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(error["error"]["type"], json!("replay_divergence"));
    assert_eq!(error["error"]["step"], json!(0));
    let expected = error["error"]["expected_digest"]
        .as_str()
        .unwrap()
        .to_owned();
    let got = error["error"]["got_digest"].as_str().unwrap().to_owned();
    assert_ne!(expected, got);
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("step 0"));

    // The recorded digest IS the tape's request digest for this layout.
    let (_, tape) = send_json(&server, "GET", "/v1/tapes/sess-llmetry", None, &[]);
    assert_eq!(tape["steps"][0]["request_digest"], json!(expected));

    let (_, report) = send_json(&server, "GET", &format!("/v1/replays/{id}"), None, &[]);
    // The divergence is kept with where the request first differed, as the
    // refusal reported it.
    assert_eq!(
        report["diverged"],
        json!({"step": 0, "expected_digest": expected, "got_digest": got,
               "difference": error["error"]["difference"]})
    );
    assert_eq!(error["error"]["difference"]["field"], json!("content"));
    assert_eq!(report["served"], json!(0), "a divergence consumes nothing");
    assert_eq!(report["calls"][0]["matched"], json!(false));

    // The right request is still served the first step.
    let (status, completion) = send_json(&server, "POST", &path, Some(&first), &openai_sdk("x"));
    assert_eq!(status, 200, "{completion}");
    assert_eq!(
        completion["choices"][0]["finish_reason"],
        json!("tool_calls")
    );
    let (_, report) = send_json(&server, "GET", &format!("/v1/replays/{id}"), None, &[]);
    assert_eq!(report["served"], json!(1));
    assert_eq!(
        report["diverged"]["step"],
        json!(0),
        "the first divergence is remembered"
    );

    // Loose mode serves the same mismatched request anyway, and records it.
    let loose = create_replay(
        &server,
        json!({"run": "sess-llmetry", "mode": "loose"}),
        &[],
    );
    let loose_path = format!(
        "{}/chat/completions",
        loose["base_urls"]["openai"].as_str().unwrap()
    );
    let (status, _) = send_json(
        &server,
        "POST",
        &loose_path,
        Some(&changed),
        &openai_sdk("x"),
    );
    assert_eq!(status, 200);
    let (_, report) = send_json(
        &server,
        "GET",
        &format!("/v1/replays/{}", loose["id"].as_str().unwrap()),
        None,
        &[],
    );
    assert_eq!(report["calls"][0]["matched"], json!(false));
    assert_eq!(report["diverged"], Value::Null);
}

#[test]
fn strict_matching_holds_across_recording_conventions() {
    let server = seeded_open_server(&[]);

    // OpenInference recording, replayed through the OpenAI SDK.
    let created = create_replay(&server, json!({"run": "sess-oi"}), &[]);
    let path = format!(
        "{}/chat/completions",
        created["base_urls"]["openai"].as_str().unwrap()
    );
    let base = json!([
        {"role": "system", "content": "You track orders."},
        {"role": "user", "content": "Where is order A-17?"},
    ]);
    let (status, completion) = send_json(
        &server,
        "POST",
        &path,
        Some(&json!({"model": "gpt-4o-mini", "messages": base})),
        &openai_sdk("x"),
    );
    assert_eq!(status, 200, "{completion}");
    let call = &completion["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(call["id"], json!("call_oi_1"));
    assert_eq!(
        call["function"]["arguments"],
        json!("{\"order_id\":\"A-17\"}")
    );
    let mut messages = base.as_array().unwrap().clone();
    messages.push(json!({"role": "assistant", "content": null, "tool_calls": [call.clone()]}));
    messages.push(json!({"role": "tool", "tool_call_id": "call_oi_1", "content": "shipped"}));
    let (status, completion) = send_json(
        &server,
        "POST",
        &path,
        Some(&json!({"model": "gpt-4o-mini", "messages": messages})),
        &openai_sdk("x"),
    );
    assert_eq!(status, 200, "{completion}");
    assert_eq!(
        completion["choices"][0]["message"]["content"],
        json!("Order A-17 has shipped.")
    );

    // OTel GenAI parts recording, replayed through the OpenAI SDK: the
    // system instructions and parts normalise to the SDK's plain messages.
    let created = create_replay(&server, json!({"run": "conv-otel"}), &[]);
    let path = format!(
        "{}/chat/completions",
        created["base_urls"]["openai"].as_str().unwrap()
    );
    let base = json!([
        {"role": "system", "content": "You plan trips."},
        {"role": "user", "content": "Find me a flight to Lisbon on Friday."},
    ]);
    let (status, completion) = send_json(
        &server,
        "POST",
        &path,
        Some(&json!({"model": "gpt-4.1", "messages": base})),
        &openai_sdk("x"),
    );
    assert_eq!(status, 200, "{completion}");
    assert_eq!(
        completion["choices"][0]["finish_reason"],
        json!("tool_calls")
    );
    let call = completion["choices"][0]["message"]["tool_calls"][0].clone();
    assert_eq!(call["function"]["name"], json!("search_flights"));
    let mut messages = base.as_array().unwrap().clone();
    messages.push(json!({"role": "assistant", "content": null, "tool_calls": [call]}));
    messages.push(json!({"role": "tool", "tool_call_id": "call_otel_1",
                         "content": "TP1350 departs 09:05"}));
    let (status, completion) = send_json(
        &server,
        "POST",
        &path,
        Some(&json!({"model": "gpt-4.1", "messages": messages})),
        &openai_sdk("x"),
    );
    assert_eq!(status, 200, "{completion}");
    assert_eq!(
        completion["choices"][0]["message"]["content"],
        json!("TP1350 leaves Friday at 09:05.")
    );
}

#[test]
fn tools_next_serves_recorded_tool_results() {
    let server = seeded_open_server(&[]);
    let created = create_replay(&server, json!({"run": "sess-oi", "mode": "loose"}), &[]);
    let path = format!("/v1/replays/{}/tools/next", created["id"].as_str().unwrap());

    let (status, body) = send_json(
        &server,
        "POST",
        &path,
        Some(&json!({"tool": "refund"})),
        &[],
    );
    // A native route: the API's own error shape, the kind beside it.
    assert_eq!(status, 410, "{body}");
    assert_eq!(body["type"], json!("replay_exhausted"));
    assert!(
        body["error"]
            .as_str()
            .is_some_and(|message| message.contains("refund")),
        "{body}"
    );

    let (status, body) = send_json(
        &server,
        "POST",
        &path,
        Some(&json!({"tool": "lookup_order"})),
        &[],
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        json!({"step": 2, "tool": "lookup_order",
               "arguments": {"order_id": "A-17"}, "result": "shipped"})
    );
    // An empty body asks for the next tool step of any name; none remain.
    let (status, _, _) = send(&server, "POST", &path, None, &[]);
    assert_eq!(status, 410);
    let (status, _) = send_json(&server, "POST", &path, Some(&json!({"name": "x"})), &[]);
    assert_eq!(status, 400, "unknown fields are refused");
}

#[test]
fn replays_and_tapes_are_isolated_by_tenant() {
    let server = Server::spawn_with_env(&[], &[("TRAZA_TOKENS", &tokens())]);
    let acme_spans = openllmetry_run();
    ingest(&server, acme_spans, Some(ACME_WRITER));

    // A read-only bound credential may create a replay of its own run; the
    // run resolves inside its tenant.
    let created = create_replay(&server, json!({"run": "sess-llmetry"}), &[bearer(ACME)]);
    let id = created["id"].as_str().unwrap().to_owned();
    let path = format!("/v1/replays/{id}/openai/v1/chat/completions");
    let (first, _) = weather_requests(false);

    // Another tenant's credential cannot see it — not even that it exists.
    let (status, body) = send_json(&server, "POST", &path, Some(&first), &openai_sdk(BETA));
    assert_eq!(status, 404, "{body}");
    assert_eq!(body["error"]["type"], json!("not_found_error"));
    let (status, _) = send_json(
        &server,
        "GET",
        &format!("/v1/replays/{id}"),
        None,
        &[bearer(BETA)],
    );
    assert_eq!(status, 404);
    let (status, _) = send_json(
        &server,
        "POST",
        &format!("/v1/replays/{id}/tools/next"),
        None,
        &[bearer(BETA)],
    );
    assert_eq!(status, 404);

    // Naming another tenant is a 403; not naming one resolves in its own,
    // where the run does not exist.
    let (status, body) = send_json(
        &server,
        "POST",
        "/v1/replays",
        Some(&json!({"run": "sess-llmetry", "tenant": "acme"})),
        &[bearer(BETA)],
    );
    assert_eq!(status, 403, "{body}");
    let (status, body) = send_json(
        &server,
        "POST",
        "/v1/replays",
        Some(&json!({"run": "sess-llmetry"})),
        &[bearer(BETA)],
    );
    assert_eq!((status, body), (404, json!({"error": "run not found"})));
    let (status, _) = send_json(
        &server,
        "GET",
        "/v1/tapes/sess-llmetry",
        None,
        &[bearer(BETA)],
    );
    assert_eq!(status, 404);
    let (status, _) = send_json(
        &server,
        "GET",
        "/v1/tapes/sess-llmetry?tenant=acme",
        None,
        &[bearer(BETA)],
    );
    assert_eq!(status, 403);

    // Its own tenant and the unbound operator may use it.
    let (status, completion) = send_json(&server, "POST", &path, Some(&first), &openai_sdk(ACME));
    assert_eq!(status, 200, "{completion}");
    let (status, report) = send_json(
        &server,
        "GET",
        &format!("/v1/replays/{id}"),
        None,
        &[bearer(OPERATOR)],
    );
    assert_eq!(status, 200);
    assert_eq!(report["served"], json!(1));
    let (status, tape) = send_json(
        &server,
        "GET",
        "/v1/tapes/sess-llmetry",
        None,
        &[bearer(ACME)],
    );
    assert_eq!(status, 200);
    assert_eq!(tape["tenant"], json!("acme"));

    // An operator-view replay may hold several tenants' data: no bound
    // credential may use it.
    let operator = create_replay(&server, json!({"run": "sess-llmetry"}), &[bearer(OPERATOR)]);
    let (status, _) = send_json(
        &server,
        "GET",
        &format!("/v1/replays/{}", operator["id"].as_str().unwrap()),
        None,
        &[bearer(ACME)],
    );
    assert_eq!(status, 404);
}

#[test]
fn an_erasure_withdraws_a_replay_holding_what_it_erased() {
    let server = seeded_open_server(&[]);
    let held = create_replay(&server, json!({"run": "sess-llmetry"}), &[]);
    let other = create_replay(&server, json!({"run": "sess-oi", "mode": "loose"}), &[]);
    let held_id = held["id"].as_str().unwrap();
    let path = format!("/v1/replays/{held_id}/openai/v1/chat/completions");
    let (first, second) = weather_requests(false);
    let (status, body) = send_json(&server, "POST", &path, Some(&first), &openai_sdk("any"));
    assert_eq!(status, 200, "{body}");

    let (status, erased) = send_json(
        &server,
        "POST",
        "/v1/erasures",
        Some(&json!({"subject": {"kind": "session", "session_id": "sess-llmetry"}})),
        &[],
    );
    assert_eq!(status, 200, "{erased}");
    let erasure = erased["id"].as_u64().expect("an erasure id");

    // The recorded answer the agent would get next was erased: refused, in
    // the provider's shape, never to be retried.
    let (status, headers, text) = send(&server, "POST", &path, Some(&second), &openai_sdk("any"));
    assert_eq!(status, 410, "{text}");
    assert_eq!(header(&headers, "x-should-retry"), Some("false"));
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["error"]["type"], json!("replay_erased"));
    assert_eq!(body["error"]["erasure"], json!(erasure));
    // The native routes answer the same, in the API's own shape.
    let (status, body) = send_json(&server, "GET", &format!("/v1/replays/{held_id}"), None, &[]);
    assert_eq!(status, 410, "{body}");
    assert_eq!(body["type"], json!("replay_erased"));
    assert!(body["error"].as_str().unwrap().contains("erased"), "{body}");
    let (status, _) = send_json(
        &server,
        "POST",
        &format!("/v1/replays/{held_id}/tools/next"),
        None,
        &[],
    );
    assert_eq!(status, 410);

    // A replay of other data is untouched.
    let (status, body) = send_json(
        &server,
        "GET",
        &format!("/v1/replays/{}", other["id"].as_str().unwrap()),
        None,
        &[],
    );
    assert_eq!(status, 200, "{body}");
}

#[test]
fn one_tenants_replays_cannot_crowd_out_another() {
    const BETA_WRITER: &str = "beta-writer-token";
    let server = Server::spawn_with_env(
        &[],
        &[(
            "TRAZA_TOKENS",
            &format!("{},rw@beta:{BETA_WRITER}", tokens()),
        )],
    );
    ingest(&server, openllmetry_run(), Some(ACME_WRITER));
    ingest(&server, openllmetry_run(), Some(BETA_WRITER));

    // A read-only credential fills its own tenant's share...
    for _ in 0..64 {
        create_replay(&server, json!({"run": "sess-llmetry"}), &[bearer(ACME)]);
    }
    let (status, headers, text) = send(
        &server,
        "POST",
        "/v1/replays",
        Some(&json!({"run": "sess-llmetry"})),
        &[bearer(ACME)],
    );
    assert_eq!(status, 429, "{text}");
    let body: Value = serde_json::from_str(&text).unwrap();
    assert!(
        body["error"].as_str().unwrap().contains("share"),
        "the refusal is about the caller's own share: {body}"
    );
    // Its retry time is its own oldest replay's expiry, an hour out.
    let retry: u64 = header(&headers, "retry-after")
        .and_then(|value| value.parse().ok())
        .expect("Retry-After");
    assert!((3000..=3600).contains(&retry), "{retry}");

    // ...and nobody else is refused for it.
    create_replay(&server, json!({"run": "sess-llmetry"}), &[bearer(BETA)]);
    create_replay(
        &server,
        json!({"run": "sess-llmetry", "tenant": "beta"}),
        &[bearer(OPERATOR)],
    );
    create_replay(&server, json!({"run": "sess-llmetry"}), &[bearer(OPERATOR)]);
}

#[test]
fn unreplayable_runs_and_bad_requests_are_refused_precisely() {
    let server = seeded_open_server(&[]);
    // Content capture off: usage but no prompt or completion.
    ingest(
        &server,
        vec![native(
            "tr-blind",
            "llm",
            None,
            "chat",
            0,
            10,
            json!({
                "gen_ai.operation.name": "chat", "gen_ai.request.model": "gpt-4o",
                "gen_ai.usage.input_tokens": 10, "gen_ai.usage.output_tokens": 3,
            }),
        )],
        None,
    );
    let (status, body) = send_json(
        &server,
        "POST",
        "/v1/replays",
        Some(&json!({"run": "trace:tr-blind"})),
        &[],
    );
    assert_eq!(status, 409, "{body}");
    let message = body["error"].as_str().unwrap();
    assert!(
        message.contains("step 0") && message.contains("no response"),
        "{message}"
    );

    for (request, needle) in [
        (json!({}), "run is required"),
        (json!({"run": ""}), "run is required"),
        (json!({"run": "sess-oi", "mode": "fuzzy"}), "mode must be"),
        (json!({"run": "sess-oi", "extra": 1}), "unknown field"),
        (
            json!({"run": "sess-oi", "tenant": 7}),
            "tenant must be a string",
        ),
    ] {
        let (status, body) = send_json(&server, "POST", "/v1/replays", Some(&request), &[]);
        assert_eq!(status, 400, "{request}: {body}");
        assert!(body["error"].as_str().unwrap().contains(needle), "{body}");
    }
    let (status, body) = send_json(
        &server,
        "POST",
        "/v1/replays",
        Some(&json!({"run": "nope"})),
        &[],
    );
    assert_eq!((status, body), (404, json!({"error": "run not found"})));

    let created = create_replay(&server, json!({"run": "sess-oi"}), &[]);
    let id = created["id"].as_str().unwrap();
    // A body without messages is the SDK caller's error, in OpenAI's shape.
    let (status, body) = send_json(
        &server,
        "POST",
        &format!("/v1/replays/{id}/openai/v1/chat/completions"),
        Some(&json!({"model": "gpt-4o"})),
        &[],
    );
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["type"], json!("invalid_request_error"));
    // Anthropic errors carry Anthropic's top-level type.
    let (status, body) = send_json(
        &server,
        "POST",
        &format!("/v1/replays/{id}/anthropic/v1/messages"),
        Some(&json!([])),
        &[],
    );
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["type"], json!("error"));
    // Only chat completions are replayed under the OpenAI base URL.
    let (status, body) = send_json(
        &server,
        "POST",
        &format!("/v1/replays/{id}/openai/v1/responses"),
        Some(&json!({})),
        &[],
    );
    assert_eq!(status, 400, "{body}");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("chat/completions"));
    // An unknown replay id.
    let (status, body) = send_json(
        &server,
        "POST",
        "/v1/replays/rp_00000000000000000000000000000000/openai/v1/chat/completions",
        Some(&json!({"messages": []})),
        &[],
    );
    assert_eq!(status, 404, "{body}");
    let (status, body) = send_json(&server, "GET", "/v1/replays/rp_nope", None, &[]);
    assert_eq!((status, body), (404, json!({"error": "replay not found"})));
}

#[test]
fn mcp_tools_read_tapes_comparisons_and_recall_over_http() {
    let server = seeded_open_server(&["--mcp"]);
    let rpc = |name: &str, arguments: Value| -> Value {
        let (status, body) = send_json(
            &server,
            "POST",
            "/v1/mcp",
            Some(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                         "params": {"name": name, "arguments": arguments}})),
            &[("Accept", "application/json, text/event-stream".into())],
        );
        assert_eq!(status, 200, "{body}");
        body["result"].clone()
    };

    let tape = rpc("get_run_tape", json!({"run_id": "sess-oi"}));
    assert_eq!(tape["isError"], json!(false), "{tape}");
    let text = tape["content"][0]["text"].as_str().unwrap();
    assert!(
        text.starts_with("Run sess-oi: 4 steps from 4 spans."),
        "{text}"
    );
    assert!(
        text.contains("<traza:telemetry untrusted=\"true\">"),
        "{text}"
    );
    assert!(text.contains("#2 tool lookup_order"), "{text}");

    let compared = rpc("compare_runs", json!({"a": "sess-llmetry", "b": "sess-oi"}));
    assert_eq!(compared["isError"], json!(false), "{compared}");
    assert_eq!(compared["structuredContent"]["a"]["steps"], json!(3));
    assert_eq!(compared["structuredContent"]["b"]["steps"], json!(4));
    assert!(compared["structuredContent"]["first_divergence"]["kind"].is_string());

    let recalled = rpc(
        "recall_runs",
        json!({"query": "order A-17", "outcome": "success"}),
    );
    assert_eq!(recalled["isError"], json!(false), "{recalled}");
    let runs: Vec<&str> = recalled["structuredContent"]["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| run["run"].as_str().unwrap())
        .collect();
    assert!(
        runs.contains(&"sess-oi") && runs.contains(&"sess-claude"),
        "{recalled}"
    );
}

#[test]
fn replays_serve_what_was_recorded_and_no_more() {
    // A tiny offload threshold makes the long message list a payload file.
    let server = Server::spawn(&["--payload-threshold-bytes", "64"]);
    let question = format!("Summarise: {}", "x".repeat(120));
    let messages = json!([{"role": "user", "parts": [{"type": "text", "content": question}]}]);
    ingest(
        &server,
        vec![
            // System instructions beside an offloaded message list.
            native(
                "tr-fidelity",
                "llm-1",
                None,
                "chat",
                0,
                10,
                json!({
                    "gen_ai.operation.name": "chat",
                    "gen_ai.system_instructions": "Be brief.",
                    "gen_ai.input.messages": messages.to_string(),
                    "gen_ai.completion.0.content": "Done.",
                }),
            ),
            // A tool whose result was never captured.
            native(
                "tr-fidelity",
                "tool-1",
                None,
                "fetch",
                20,
                30,
                json!({
                    "gen_ai.operation.name": "execute_tool", "gen_ai.tool.name": "fetch",
                    "gen_ai.tool.call.arguments": "{\"u\":1}",
                }),
            ),
            // Answer text that reads as a JSON message.
            native(
                "tr-fidelity",
                "llm-2",
                None,
                "chat",
                40,
                50,
                json!({
                    "gen_ai.operation.name": "chat",
                    "gen_ai.system_instructions": "Be brief.",
                    "gen_ai.input.messages": messages.to_string(),
                    "llm.completion": "{\"content\":\"hello\",\"count\":2}",
                }),
            ),
        ],
        None,
    );
    let created = create_replay(&server, json!({"run": "trace:tr-fidelity"}), &[]);
    assert_eq!(created["unreadable_tool_steps"], json!([1]), "{created}");
    let path = format!(
        "{}/chat/completions",
        created["base_urls"]["openai"].as_str().unwrap()
    );

    // The recorded system prompt is part of the recorded request.
    let user = json!({"role": "user", "content": question});
    let (status, refused) = send_json(
        &server,
        "POST",
        &path,
        Some(&json!({"model": "m", "messages": [user]})),
        &openai_sdk("x"),
    );
    assert_eq!(status, 409, "{refused}");
    assert_eq!(refused["error"]["type"], json!("replay_divergence"));
    let asked = json!({"model": "m", "messages": [
        {"role": "system", "content": "Be brief."}, user]});
    let (status, answer) = send_json(&server, "POST", &path, Some(&asked), &openai_sdk("x"));
    assert_eq!(status, 200, "{answer}");
    assert_eq!(answer["choices"][0]["message"]["content"], json!("Done."));

    // A model no provider would name is not echoed into every chunk.
    let mut streamed = asked.clone();
    streamed["stream"] = json!(true);
    streamed["model"] = json!("m".repeat(64 * 1024));
    let (status, _, body) = send(&server, "POST", &path, Some(&streamed), &openai_sdk("x"));
    assert_eq!(status, 200, "{body}");
    assert!(body.len() < 16 * 1024, "{} bytes", body.len());
    let mut text = String::new();
    for (_, data) in sse(&body) {
        if data == "[DONE]" {
            continue;
        }
        let event: Value = serde_json::from_str(&data).unwrap();
        assert_eq!(event["model"], json!("unknown"));
        text.push_str(
            event["choices"][0]["delta"]["content"]
                .as_str()
                .unwrap_or_default(),
        );
    }
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap(),
        json!({"content": "hello", "count": 2}),
        "the answer's text, not a message read out of it"
    );

    // The uncaptured tool result is refused, and stays refused.
    let tools = format!("/v1/replays/{}/tools/next", created["id"].as_str().unwrap());
    for _ in 0..2 {
        let (status, headers, body) = send(&server, "POST", &tools, None, &[]);
        assert_eq!(status, 409, "{body}");
        assert_eq!(header(&headers, "x-should-retry"), Some("false"));
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["type"], json!("tool_unreadable"));
        assert!(
            body["error"]
                .as_str()
                .is_some_and(|message| message.contains("recorded no result")),
            "{body}"
        );
    }
}
