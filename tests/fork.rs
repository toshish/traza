//! Fork & Rewind, end to end against the real binary: a replay told to serve
//! an edited answer or tool result at one step, to hand the run to the real
//! provider after another, and to explain a divergence; then the forks a
//! harness recorded found again and diffed against their parent, and the
//! runs marked golden listed for a test runner — all under tenant binding.
//!
//! Runs are seeded the way OpenLLMetry's OpenAI instrumentation records them
//! (indexed `gen_ai.prompt.{i}.*`), and the provider routes are driven with
//! raw HTTP shaped as the official SDKs send it.

mod common;

use common::{header, Server};
use serde_json::{json, Value};

/// 2026-01-01T00:00:00Z: old enough that every seeded run reads as finished.
const T0: u64 = 1_767_225_600_000_000_000;
const MS: u64 = 1_000_000;

const OPERATOR: &str = "operator-token";
const ACME: &str = "acme-reader-token";
const ACME_WRITER: &str = "acme-writer-token";
const BETA: &str = "beta-reader-token";
const BETA_WRITER: &str = "beta-writer-token";

const SYSTEM: &str = "You are a weather assistant.";
const USER: &str = "What's the weather in Paris?";

fn tokens() -> String {
    format!(
        "admin:{OPERATOR},ro@acme:{ACME},rw@acme:{ACME_WRITER},ro@beta:{BETA},\
         rw@beta:{BETA_WRITER}"
    )
}

fn server(args: &[&str]) -> Server {
    Server::spawn_with_env(args, &[("TRAZA_TOKENS", &tokens())])
}

fn bearer(token: &str) -> (&'static str, String) {
    ("Authorization", format!("Bearer {token}"))
}

fn call(
    server: &Server,
    method: &str,
    path: &str,
    body: Option<&Value>,
    headers: &[(&'static str, String)],
) -> (u16, Vec<(String, String)>, Value) {
    let borrowed: Vec<(&str, &str)> = headers
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    let encoded = body.map(|value| serde_json::to_vec(value).expect("encodes"));
    let (status, head, text) = server.request(method, path, encoded.as_deref(), &borrowed);
    let parsed = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("{path} answered {status} non-JSON ({error}): {text}"))
    };
    (status, head, parsed)
}

fn as_token(
    server: &Server,
    method: &str,
    path: &str,
    body: Option<&Value>,
    token: &str,
) -> (u16, Value) {
    let (status, _, body) = call(server, method, path, body, &[bearer(token)]);
    (status, body)
}

fn span(trace: &str, id: &str, name: &str, start_ms: u64, end_ms: u64, attributes: Value) -> Value {
    json!({
        "trace_id": trace, "span_id": id, "parent_span_id": null, "name": name,
        "service": "weather-agent", "status": "ok",
        "start_time_ns": T0 + start_ms * MS, "end_time_ns": T0 + end_ms * MS,
        "attributes": attributes,
    })
}

/// The weather agent's run as OpenLLMetry records it: a model call asking
/// for `get_weather`, the tool, and the model call that answers. `extra` is
/// added to every span — a fork's tags, a golden marker. `forecast` is what
/// the tool returned.
fn weather_run(session: &str, trace: &str, at_ms: u64, forecast: &str, extra: Value) -> Vec<Value> {
    let result = format!("{{\"forecast\": \"{forecast}\", \"celsius\": 12}}");
    let mut spans = vec![
        span(
            trace,
            "llm-1",
            "openai.chat",
            at_ms,
            at_ms + 400,
            json!({
                "gen_ai.system": "openai", "llm.request.type": "chat",
                "gen_ai.request.model": "gpt-4o",
                "gen_ai.response.model": "gpt-4o-2024-08-06",
                "gen_ai.response.id": "chatcmpl-AbC123",
                "gen_ai.usage.prompt_tokens": 82, "gen_ai.usage.completion_tokens": 17,
                "gen_ai.prompt.0.role": "system", "gen_ai.prompt.0.content": SYSTEM,
                "gen_ai.prompt.1.role": "user", "gen_ai.prompt.1.content": USER,
                "gen_ai.completion.0.role": "assistant",
                "gen_ai.completion.0.finish_reason": "tool_calls",
                "gen_ai.completion.0.tool_calls.0.id": "call_w1",
                "gen_ai.completion.0.tool_calls.0.name": "get_weather",
                "gen_ai.completion.0.tool_calls.0.arguments": "{\"city\": \"Paris\"}",
            }),
        ),
        span(
            trace,
            "tool-1",
            "get_weather.tool",
            at_ms + 410,
            at_ms + 450,
            json!({
                "traceloop.span.kind": "tool", "traceloop.entity.name": "get_weather",
                "traceloop.entity.input": "{\"city\": \"Paris\"}",
                "traceloop.entity.output": result,
            }),
        ),
        span(
            trace,
            "llm-2",
            "openai.chat",
            at_ms + 460,
            at_ms + 900,
            json!({
                "gen_ai.system": "openai", "llm.request.type": "chat",
                "gen_ai.request.model": "gpt-4o",
                "gen_ai.response.model": "gpt-4o-2024-08-06",
                "gen_ai.usage.prompt_tokens": 120, "gen_ai.usage.completion_tokens": 14,
                "gen_ai.prompt.0.role": "system", "gen_ai.prompt.0.content": SYSTEM,
                "gen_ai.prompt.1.role": "user", "gen_ai.prompt.1.content": USER,
                "gen_ai.prompt.2.role": "assistant",
                "gen_ai.prompt.2.tool_calls.0.id": "call_w1",
                "gen_ai.prompt.2.tool_calls.0.name": "get_weather",
                "gen_ai.prompt.2.tool_calls.0.arguments": "{\"city\": \"Paris\"}",
                "gen_ai.prompt.3.role": "tool", "gen_ai.prompt.3.tool_call_id": "call_w1",
                "gen_ai.prompt.3.content": result,
                "gen_ai.completion.0.role": "assistant",
                "gen_ai.completion.0.finish_reason": "stop",
                "gen_ai.completion.0.content": format!("It's {forecast} in Paris, 12°C."),
            }),
        ),
    ];
    for span in &mut spans {
        span["attributes"]["session.id"] = json!(session);
        if let (Some(target), Some(more)) = (span["attributes"].as_object_mut(), extra.as_object())
        {
            for (key, value) in more {
                target.insert(key.clone(), value.clone());
            }
        }
    }
    spans
}

fn ingest(server: &Server, spans: Vec<Value>, token: &str) {
    let (status, body) = as_token(server, "POST", "/v1/spans", Some(&json!(spans)), token);
    assert_eq!(status, 200, "{body}");
}

/// The two requests the weather agent's OpenAI SDK sends.
fn first_request() -> Value {
    json!({"model": "gpt-4o", "messages": [
        {"role": "system", "content": SYSTEM},
        {"role": "user", "content": USER},
    ]})
}

fn second_request(tool_result: &str) -> Value {
    json!({"model": "gpt-4o", "messages": [
        {"role": "system", "content": SYSTEM},
        {"role": "user", "content": USER},
        {"role": "assistant", "content": null, "tool_calls": [{
            "id": "call_w1", "type": "function",
            "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}]},
        {"role": "tool", "tool_call_id": "call_w1", "content": tool_result},
    ]})
}

fn openai_sdk(token: &str) -> Vec<(&'static str, String)> {
    vec![
        bearer(token),
        ("User-Agent", "OpenAI/Python 1.51.0".into()),
        ("x-stainless-retry-count", "0".into()),
    ]
}

fn chat(
    server: &Server,
    replay: &str,
    body: &Value,
    token: &str,
) -> (u16, Vec<(String, String)>, Value) {
    call(
        server,
        "POST",
        &format!("/v1/replays/{replay}/openai/v1/chat/completions"),
        Some(body),
        &openai_sdk(token),
    )
}

fn seeded() -> Server {
    let server = server(&[]);
    ingest(
        &server,
        weather_run("weather-1", "tr-parent", 0, "rain", json!({})),
        ACME_WRITER,
    );
    server
}

#[test]
fn a_fork_serves_its_edit_then_hands_the_run_to_the_real_provider() {
    let server = seeded();
    // The model is told it asked about Lyon instead, and after that answer
    // the run is the real provider's.
    let (status, created) = as_token(
        &server,
        "POST",
        "/v1/replays",
        Some(&json!({
            "run": "weather-1",
            "edits": [{"step": 0, "response": {"content": null, "tool_calls": [{
                "id": "call_lyon", "type": "function",
                "function": {"name": "get_weather", "arguments": "{\"city\":\"Lyon\"}"}}]}}],
            "live_after": 0,
        })),
        ACME,
    );
    assert_eq!(status, 201, "{created}");
    let replay = created["id"].as_str().unwrap().to_owned();
    let fork_id = created["fork_id"].as_str().expect("a fork id").to_owned();
    assert!(
        fork_id.starts_with("fk_") && fork_id.len() == 35,
        "{fork_id}"
    );
    assert_ne!(
        fork_id[3..],
        replay[3..],
        "the fork id is not the replay id"
    );
    assert_eq!(
        created["llm_steps"],
        json!(1),
        "nothing past the live point is held"
    );
    assert_eq!(
        created["fork"],
        json!({"id": fork_id, "parent": "weather-1", "step": 0,
               "edits": [{"step": 0, "kind": "response"}], "live_after": 0,
               "attributes": {"traza.fork.parent": "weather-1", "traza.fork.step": 0,
                              "traza.fork.id": fork_id}})
    );

    let (status, _, answer) = chat(&server, &replay, &first_request(), ACME);
    assert_eq!(status, 200, "{answer}");
    let tool_call = &answer["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(tool_call["id"], json!("call_lyon"));
    assert_eq!(
        tool_call["function"]["arguments"],
        json!("{\"city\":\"Lyon\"}")
    );
    assert_eq!(answer["choices"][0]["finish_reason"], json!("tool_calls"));
    assert_ne!(
        answer["id"],
        json!("chatcmpl-AbC123"),
        "not the recorded answer's id"
    );
    assert_eq!(
        answer["usage"]["total_tokens"],
        json!(0),
        "no provider produced it"
    );

    // Past the live point: the OpenAI route, the Anthropic route and the tool
    // route all say so, in their own shapes, and none of them is retried.
    let (status, head, refused) = chat(&server, &replay, &second_request("x"), ACME);
    assert_eq!(status, 409, "{refused}");
    assert_eq!(header(&head, "x-should-retry"), Some("false"));
    assert_eq!(refused["error"]["type"], json!("replay_live"));
    assert_eq!(refused["error"]["live_after"], json!(0));
    assert_eq!(refused["error"]["served"], json!(1));
    assert_eq!(refused["error"]["fork_id"], json!(fork_id));
    assert!(refused["error"]["message"]
        .as_str()
        .unwrap()
        .contains("real provider"));
    let (status, _, refused) = call(
        &server,
        "POST",
        &format!("/v1/replays/{replay}/anthropic/v1/messages"),
        Some(
            &json!({"model": "claude-x", "max_tokens": 64, "messages": [{"role": "user", "content": "hi"}]}),
        ),
        &[
            ("x-api-key", ACME.to_owned()),
            ("anthropic-version", "2023-06-01".to_owned()),
        ],
    );
    assert_eq!(status, 409);
    assert_eq!(refused["type"], json!("error"));
    assert_eq!(refused["error"]["type"], json!("replay_live"));
    let (status, refused) = as_token(
        &server,
        "POST",
        &format!("/v1/replays/{replay}/tools/next"),
        Some(&json!({"tool": "get_weather"})),
        ACME,
    );
    assert_eq!(status, 409);
    assert_eq!(refused["type"], json!("replay_live"));
    assert!(refused["error"].is_string());

    let (status, report) = as_token(&server, "GET", &format!("/v1/replays/{replay}"), None, ACME);
    assert_eq!(status, 200);
    assert_eq!(report["fork_id"], json!(fork_id));
    assert_eq!(
        report["fork"]["edits"],
        json!([{"step": 0, "kind": "response", "served": true}])
    );
    assert_eq!(report["fork"]["live_calls"], json!(3));
    assert!(report["fork"]["live_at_ns"].as_u64().is_some());
    assert_eq!(report["exhausted_calls"], json!(0));

    // Another tenant's credential cannot use it.
    let (status, _, _) = chat(&server, &replay, &first_request(), BETA);
    assert_eq!(status, 404);
}

#[test]
fn a_tool_result_edit_flows_back_into_the_conversation() {
    let server = seeded();
    let (status, created) = as_token(
        &server,
        "POST",
        "/v1/replays",
        Some(&json!({"run": "weather-1",
                     "edits": [{"step": 1, "tool_result": {"forecast": "sun", "celsius": 30}}]})),
        ACME,
    );
    assert_eq!(status, 201, "{created}");
    assert_eq!(created["fork"]["step"], json!(1));
    assert_eq!(created["fork"]["live_after"], Value::Null);
    let replay = created["id"].as_str().unwrap().to_owned();

    let (status, _, _) = chat(&server, &replay, &first_request(), ACME);
    assert_eq!(
        status, 200,
        "before the edit the recording is served, strictly"
    );
    let (status, tool) = as_token(
        &server,
        "POST",
        &format!("/v1/replays/{replay}/tools/next"),
        Some(&json!({})),
        ACME,
    );
    assert_eq!(status, 200);
    assert_eq!(
        tool,
        json!({"step": 1, "tool": "get_weather", "arguments": {"city": "Paris"},
               "result": {"forecast": "sun", "celsius": 30}, "edited": true})
    );
    // The agent sends the edited result back: not what was recorded, and
    // served all the same, because the run is on a new branch now.
    let edited = second_request("{\"forecast\":\"sun\",\"celsius\":30}");
    let (status, _, answer) = chat(&server, &replay, &edited, ACME);
    assert_eq!(status, 200, "{answer}");
    assert_eq!(
        answer["choices"][0]["message"]["content"],
        json!("It's rain in Paris, 12°C."),
        "after an edit without a live point the recording plays on"
    );
    let (_, report) = as_token(&server, "GET", &format!("/v1/replays/{replay}"), None, ACME);
    let matched: Vec<Value> = report["calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|call| call["matched"].clone())
        .collect();
    assert_eq!(matched, [json!(true), json!(false)]);
    assert_eq!(report["diverged"], Value::Null);
    assert_eq!(
        report["fork"]["edits"],
        json!([{"step": 1, "kind": "tool_result", "served": true}])
    );
}

#[test]
fn a_rewind_replays_the_beginning_and_goes_live_where_asked() {
    let server = seeded();
    let (status, created) = as_token(
        &server,
        "POST",
        "/v1/replays",
        Some(&json!({"run": "weather-1", "live_after": 1})),
        ACME,
    );
    assert_eq!(status, 201, "{created}");
    assert_eq!(
        created["fork"]["step"],
        json!(2),
        "cut at the step after the live point"
    );
    assert_eq!(created["fork"]["edits"], json!([]));
    let replay = created["id"].as_str().unwrap().to_owned();
    // Strict all the way to the live point.
    let mut wrong = first_request();
    wrong["messages"][1]["content"] = json!("What's the weather in Rome?");
    let (status, _, refused) = chat(&server, &replay, &wrong, ACME);
    assert_eq!(
        (status, refused["error"]["type"].clone()),
        (409, json!("replay_divergence"))
    );
    assert_eq!(chat(&server, &replay, &first_request(), ACME).0, 200);
    let (status, tool) = as_token(
        &server,
        "POST",
        &format!("/v1/replays/{replay}/tools/next"),
        None,
        ACME,
    );
    assert_eq!(
        (status, tool["result"]["forecast"].clone()),
        (200, json!("rain"))
    );
    let (status, _, refused) = chat(&server, &replay, &second_request("x"), ACME);
    assert_eq!(
        (status, refused["error"]["type"].clone()),
        (409, json!("replay_live"))
    );
}

#[test]
fn fork_requests_the_run_cannot_take_are_refused_with_reasons() {
    let server = seeded();
    let refused = |body: Value| {
        let (status, answer) = as_token(&server, "POST", "/v1/replays", Some(&body), ACME);
        assert_eq!(status, 400, "{body} -> {answer}");
        answer["error"]
            .as_str()
            .expect("an error message")
            .to_owned()
    };
    let wrong_kind = refused(json!({"run": "weather-1", "edits": [{"step": 1, "response": "hi"}]}));
    assert!(
        wrong_kind.contains("step 1 is a tool call (tool:get_weather)")
            && wrong_kind.contains("tool_result"),
        "{wrong_kind}"
    );
    assert!(
        refused(json!({"run": "weather-1", "edits": [{"step": 9, "tool_result": 1}]}))
            .contains("past the run's last step (2)")
    );
    assert!(refused(json!({"run": "weather-1", "live_after": 3})).contains("live_after 3 is past"));
    assert!(refused(json!({"run": "weather-1", "live_after": 0,
                           "edits": [{"step": 2, "response": "late"}]}))
    .contains("comes after live_after 0"));
    assert!(
        refused(json!({"run": "weather-1", "edits": [{"step": 0, "reply": "x"}]}))
            .contains("unknown field: reply")
    );
    assert!(refused(json!({"run": "weather-1", "edits": {"step": 0}})).contains("must be a list"));
    assert!(refused(json!({"run": "weather-1", "live_after": "1"})).contains("live_after must be"));
    assert!(
        refused(json!({"run": "weather-1", "edits": [{"step": 0, "response": {"n": 1}}]}))
            .contains("the edit at step 0")
    );
    // A plain replay is no fork: its creation carries no fork fields.
    let (status, plain) = as_token(
        &server,
        "POST",
        "/v1/replays",
        Some(&json!({"run": "weather-1"})),
        ACME,
    );
    assert_eq!(status, 201);
    assert!(
        plain.get("fork_id").is_none() && plain.get("fork").is_none(),
        "{plain}"
    );
    // The run is another tenant's: a bound credential cannot fork it.
    let (status, _) = as_token(
        &server,
        "POST",
        "/v1/replays",
        Some(&json!({"run": "weather-1", "live_after": 0})),
        BETA,
    );
    assert_eq!(status, 404);
}

#[test]
fn a_divergence_says_which_message_differs_and_where() {
    let server = seeded();
    let (_, created) = as_token(
        &server,
        "POST",
        "/v1/replays",
        Some(&json!({"run": "weather-1"})),
        ACME,
    );
    let replay = created["id"].as_str().unwrap().to_owned();
    let mut wrong = first_request();
    wrong["messages"][1]["content"] = json!("What's the weather in Lyon?");
    let (status, head, refused) = chat(&server, &replay, &wrong, ACME);
    assert_eq!(status, 409);
    assert_eq!(header(&head, "x-should-retry"), Some("false"));
    assert_eq!(refused["error"]["type"], json!("replay_divergence"));
    let difference = refused["error"]["difference"].clone();
    assert_eq!(
        difference,
        json!({"message": 1, "role": "user", "field": "content", "at_char": 22,
               "recorded": USER, "got": "What's the weather in Lyon?",
               "recorded_messages": 2, "got_messages": 2})
    );
    let (_, report) = as_token(&server, "GET", &format!("/v1/replays/{replay}"), None, ACME);
    assert_eq!(report["diverged"]["difference"], difference);

    // An Anthropic request: its system prompt is the leading message, and a
    // missing one is named as missing.
    let (status, _, refused) = call(
        &server,
        "POST",
        &format!("/v1/replays/{replay}/anthropic/v1/messages"),
        Some(&json!({"model": "claude-x", "max_tokens": 64,
                     "messages": [{"role": "user", "content": USER}]})),
        &[
            ("x-api-key", ACME.to_owned()),
            ("anthropic-version", "2023-06-01".to_owned()),
        ],
    );
    assert_eq!(status, 409);
    let difference = &refused["error"]["difference"];
    assert_eq!(difference["message"], json!(0));
    assert_eq!(difference["field"], json!("role"), "{difference}");
    assert_eq!(difference["recorded"], json!("system"));
    assert_eq!(difference["got"], json!("user"));
    assert_eq!(
        (
            difference["recorded_messages"].clone(),
            difference["got_messages"].clone()
        ),
        (json!(2), json!(1))
    );
}

#[test]
fn a_kept_difference_is_bounded_whatever_the_request_holds() {
    let server = seeded();
    let (_, created) = as_token(
        &server,
        "POST",
        "/v1/replays",
        Some(&json!({"run": "weather-1"})),
        ACME,
    );
    let replay = created["id"].as_str().unwrap().to_owned();
    // The recorded messages plus an extra one whose role is a megabyte: a
    // read-only credential replaying its own run chooses every byte of it.
    let mut padded = first_request();
    padded["messages"]
        .as_array_mut()
        .unwrap()
        .push(json!({"role": "r".repeat(1 << 20), "content": "x".repeat(1 << 20)}));
    let (status, _, refused) = chat(&server, &replay, &padded, ACME);
    assert_eq!(status, 409, "{}", refused["error"]["type"]);
    let difference = refused["error"]["difference"].clone();
    assert_eq!(difference["field"], json!("extra"), "{difference}");
    let role = difference["role"].as_str().unwrap();
    assert!(
        role.chars().count() <= 33 && role.ends_with('…'),
        "a role of {} characters was kept",
        role.chars().count()
    );
    assert!(difference["got"].as_str().unwrap().chars().count() <= 122);
    let kept = serde_json::to_vec(&difference).unwrap().len();
    assert!(kept < 1024, "{kept} bytes kept for the replay's lifetime");
    let (_, report) = as_token(&server, "GET", &format!("/v1/replays/{replay}"), None, ACME);
    assert_eq!(report["diverged"]["difference"], difference);
}

#[test]
fn a_replay_stops_rereading_the_recording_to_explain_divergences() {
    let server = seeded();
    let (_, created) = as_token(
        &server,
        "POST",
        "/v1/replays",
        Some(&json!({"run": "weather-1"})),
        ACME,
    );
    let replay = created["id"].as_str().unwrap().to_owned();
    let asked = |attempt: usize| {
        let mut wrong = first_request();
        wrong["messages"][1]["content"] = json!(format!("What's the weather at {attempt}?"));
        wrong
    };
    // A harness whose prompt carries a timestamp: every retry is a new
    // request, and each costs a read of the recording — eight at most.
    for attempt in 0..8 {
        let (status, _, refused) = chat(&server, &replay, &asked(attempt), ACME);
        assert_eq!(status, 409);
        assert_eq!(
            refused["error"]["difference"]["field"],
            json!("content"),
            "attempt {attempt}: {refused}"
        );
    }
    let (status, head, refused) = chat(&server, &replay, &asked(8), ACME);
    assert_eq!(status, 409);
    assert_eq!(header(&head, "x-should-retry"), Some("false"));
    assert_eq!(refused["error"]["type"], json!("replay_divergence"));
    assert!(
        refused["error"].get("difference").is_none(),
        "past the cap the refusal stands without a read: {refused}"
    );
    // The first divergence's explanation was kept, and still answers.
    let (_, _, again) = chat(&server, &replay, &asked(0), ACME);
    assert_eq!(
        again["error"]["difference"]["got"],
        json!("What's the weather at 0?")
    );
    let (_, report) = as_token(&server, "GET", &format!("/v1/replays/{replay}"), None, ACME);
    assert_eq!(
        report["diverged"]["difference"],
        again["error"]["difference"]
    );
}

/// A fork of `weather-1` as a harness records it: its own session, every
/// span tagged with the parent, the cut step and the fork id.
fn recorded_fork(
    session: &str,
    trace: &str,
    at_ms: u64,
    forecast: &str,
    fork_id: &str,
) -> Vec<Value> {
    weather_run(
        session,
        trace,
        at_ms,
        forecast,
        json!({"traza.fork.parent": "weather-1", "traza.fork.step": 1, "traza.fork.id": fork_id}),
    )
}

#[test]
fn forks_are_listed_with_where_they_parted_and_stay_in_their_tenant() {
    let server = seeded();
    ingest(
        &server,
        recorded_fork("weather-1-sunny", "tr-f1", 5_000, "sun", "fk_sunny"),
        ACME_WRITER,
    );
    ingest(
        &server,
        recorded_fork("weather-1-same", "tr-f2", 9_000, "rain", "fk_same"),
        ACME_WRITER,
    );
    // Another tenant reuses the run id, with a fork of its own.
    ingest(
        &server,
        weather_run("weather-1", "tr-beta", 0, "snow", json!({})),
        BETA_WRITER,
    );
    ingest(
        &server,
        recorded_fork("beta-fork", "tr-bf", 7_000, "hail", "fk_beta"),
        BETA_WRITER,
    );

    let (status, listing) = as_token(&server, "GET", "/v1/forks?run=weather-1", None, ACME);
    assert_eq!(status, 200, "{listing}");
    assert_eq!(listing["run"], json!("weather-1"));
    assert_eq!(listing["parent"]["steps"], json!(3));
    assert_eq!(listing["parent"]["outcome"], json!("success"));
    assert_eq!(listing["truncated"], json!(false));
    assert_eq!(listing["merged_into_parent"], json!(0));
    let forks = listing["forks"].as_array().unwrap();
    let runs: Vec<&str> = forks
        .iter()
        .map(|fork| fork["run"].as_str().unwrap())
        .collect();
    assert_eq!(
        runs,
        ["weather-1-same", "weather-1-sunny"],
        "newest first, acme's only"
    );
    let same = &forks[0];
    assert_eq!(same["fork_id"], json!("fk_same"));
    assert_eq!(
        same["first_divergence"],
        Value::Null,
        "it re-ran the parent exactly"
    );
    assert_eq!(same["diverged_before_cut"], json!(false));
    let sunny = &forks[1];
    assert_eq!(sunny["tenant"], json!("acme"));
    assert_eq!(sunny["fork_step"], json!(1));
    assert_eq!(sunny["tagged_spans"], json!(3));
    assert_eq!(sunny["diffed"], json!(true));
    assert_eq!(sunny["outcome"], json!("success"));
    assert_eq!(sunny["steps"], json!(3));
    let first = &sunny["first_divergence"];
    assert_eq!(
        (first["a"].clone(), first["b"].clone()),
        (json!(1), json!(1))
    );
    assert_eq!(
        first["kind"],
        json!("decision"),
        "same tool input, another result: {first}"
    );
    assert_eq!(sunny["diverged_before_cut"], json!(false));

    // Beta sees its own fork of its own run, nothing of acme's.
    let (status, listing) = as_token(&server, "GET", "/v1/forks?run=weather-1", None, BETA);
    assert_eq!(status, 200);
    let runs: Vec<&str> = listing["forks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|fork| fork["run"].as_str().unwrap())
        .collect();
    assert_eq!(runs, ["beta-fork"]);
    let (status, refused) = as_token(
        &server,
        "GET",
        "/v1/forks?run=weather-1&tenant=acme",
        None,
        BETA,
    );
    assert_eq!(
        (status, refused["required"].clone()),
        (403, json!("unbound"))
    );
    // The operator sees every tenant's forks, each diffed in its own tenant.
    let (status, listing) = as_token(&server, "GET", "/v1/forks?run=weather-1", None, OPERATOR);
    assert_eq!(status, 200);
    let tenants: Vec<(&str, &str, Value)> = listing["forks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|fork| {
            (
                fork["run"].as_str().unwrap(),
                fork["tenant"].as_str().unwrap(),
                fork["first_divergence"]["b"].clone(),
            )
        })
        .collect();
    assert_eq!(
        tenants,
        [
            ("weather-1-same", "acme", Value::Null),
            ("beta-fork", "beta", json!(1)),
            ("weather-1-sunny", "acme", json!(1)),
        ]
    );
    // Bounded by the limit, and saying so.
    let (_, listing) = as_token(
        &server,
        "GET",
        "/v1/forks?run=weather-1&limit=1",
        None,
        ACME,
    );
    assert_eq!(listing["forks"].as_array().unwrap().len(), 1);
    assert_eq!(listing["truncated"], json!(true));

    // Refusals.
    assert_eq!(
        as_token(&server, "GET", "/v1/forks?run=nope", None, ACME).0,
        404
    );
    assert_eq!(as_token(&server, "GET", "/v1/forks", None, ACME).0, 400);
    assert_eq!(
        as_token(
            &server,
            "GET",
            "/v1/forks?run=weather-1&limit=0",
            None,
            ACME
        )
        .0,
        400
    );
    let (status, body) = as_token(
        &server,
        "GET",
        "/v1/forks?run=weather-1&depth=2",
        None,
        ACME,
    );
    assert_eq!(
        (status, body["error"].clone()),
        (400, json!("unknown query parameter: depth"))
    );
}

#[test]
fn a_fork_that_kept_its_parents_session_is_counted_not_listed() {
    let server = seeded();
    // The harness tagged its spans but reused the parent's session id, so
    // they joined the parent run.
    ingest(
        &server,
        weather_run(
            "weather-1",
            "tr-stray",
            3_000,
            "sun",
            json!({"traza.fork.parent": "weather-1"}),
        ),
        ACME_WRITER,
    );
    let (status, listing) = as_token(&server, "GET", "/v1/forks?run=weather-1", None, ACME);
    assert_eq!(status, 200, "{listing}");
    assert_eq!(listing["forks"], json!([]));
    assert_eq!(listing["merged_into_parent"], json!(3));
}

#[test]
fn goldens_list_tagged_and_annotated_runs_per_tenant() {
    let server = server(&[]);
    ingest(
        &server,
        weather_run(
            "golden-by-tag",
            "tr-g1",
            0,
            "rain",
            json!({"traza.golden": true}),
        ),
        ACME_WRITER,
    );
    ingest(
        &server,
        weather_run("golden-by-note", "tr-g2", 2_000, "rain", json!({})),
        ACME_WRITER,
    );
    ingest(
        &server,
        weather_run("ordinary", "tr-g3", 4_000, "rain", json!({})),
        ACME_WRITER,
    );
    ingest(
        &server,
        weather_run(
            "beta-golden",
            "tr-g4",
            0,
            "rain",
            json!({"traza.golden": "true"}),
        ),
        BETA_WRITER,
    );
    let mark = |session: &str, value: Value, token: &str| {
        let (status, body) = as_token(
            &server,
            "POST",
            "/v1/annotations",
            Some(
                &json!({"session_id": session, "name": "traza.golden", "value": value,
                         "source": "human:tester"}),
            ),
            token,
        );
        assert_eq!(status, 200, "{body}");
    };
    mark("golden-by-note", json!(true), ACME_WRITER);

    let listed = |token: &str| -> Vec<(String, String, String)> {
        let (status, body) = as_token(&server, "GET", "/v1/goldens", None, token);
        assert_eq!(status, 200, "{body}");
        body["runs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|run| {
                (
                    run["tenant"].as_str().unwrap().to_owned(),
                    run["run"].as_str().unwrap().to_owned(),
                    run["source"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    };
    let owned = |rows: &[(&str, &str, &str)]| -> Vec<(String, String, String)> {
        rows.iter()
            .map(|(a, b, c)| ((*a).to_owned(), (*b).to_owned(), (*c).to_owned()))
            .collect()
    };
    assert_eq!(
        listed(ACME),
        owned(&[
            ("acme", "golden-by-note", "annotation"),
            ("acme", "golden-by-tag", "attribute")
        ])
    );
    assert_eq!(listed(BETA), owned(&[("beta", "beta-golden", "attribute")]));
    assert_eq!(listed(OPERATOR).len(), 3);
    // One rule on both paths: the boolean or exactly "true". `str(True)` in
    // Python marks nothing, as a span attribute or as an annotation.
    ingest(
        &server,
        weather_run(
            "cased-tag",
            "tr-g5",
            6_000,
            "rain",
            json!({"traza.golden": "True"}),
        ),
        ACME_WRITER,
    );
    mark("ordinary", json!("True"), ACME_WRITER);
    assert_eq!(
        listed(ACME),
        owned(&[
            ("acme", "golden-by-note", "annotation"),
            ("acme", "golden-by-tag", "attribute")
        ])
    );
    // A later "false" un-marks a run, whatever its spans say.
    mark("golden-by-tag", json!(false), ACME_WRITER);
    assert_eq!(
        listed(ACME),
        owned(&[("acme", "golden-by-note", "annotation")])
    );
    let (status, _) = as_token(&server, "GET", "/v1/goldens?tenant=acme", None, BETA);
    assert_eq!(status, 403);
    assert_eq!(
        as_token(&server, "GET", "/v1/goldens?limit=x", None, ACME).0,
        400
    );
}

#[test]
fn the_mcp_tool_lists_forks_inside_the_untrusted_block() {
    let server = server(&["--mcp"]);
    ingest(
        &server,
        weather_run("weather-1", "tr-parent", 0, "rain", json!({})),
        ACME_WRITER,
    );
    ingest(
        &server,
        recorded_fork("weather-1-sunny", "tr-f1", 5_000, "sun", "fk_sunny"),
        ACME_WRITER,
    );
    let (status, _, body) = call(
        &server,
        "POST",
        "/v1/mcp",
        Some(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                     "params": {"name": "list_forks", "arguments": {"run_id": "weather-1"}}})),
        &[
            bearer(ACME),
            ("Accept", "application/json, text/event-stream".into()),
        ],
    );
    assert_eq!(status, 200, "{body}");
    let result = &body["result"];
    assert_eq!(result["isError"], json!(false), "{result}");
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(
        text.starts_with("Run weather-1 has 1 recorded fork."),
        "{text}"
    );
    let open = text
        .find("<traza:telemetry untrusted=\"true\">")
        .expect("block");
    let row = text
        .find("weather-1-sunny [fk_sunny] · cut at step 1 · success")
        .expect("row");
    let close = text.find("</traza:telemetry>").expect("closed");
    assert!(open < row && row < close, "{text}");
    assert!(text.contains("parted at step 1 (decision)"), "{text}");
    // Bound to beta, the same question finds nothing.
    let (_, _, body) = call(
        &server,
        "POST",
        "/v1/mcp",
        Some(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                     "params": {"name": "list_forks", "arguments": {"run_id": "weather-1"}}})),
        &[
            bearer(BETA),
            ("Accept", "application/json, text/event-stream".into()),
        ],
    );
    assert_eq!(body["result"]["isError"], json!(true), "{body}");
}

#[test]
fn a_fork_listing_never_outlives_its_query_budget() {
    let server = server(&["--query-deadline-ms", "1", "--flush-spans", "100000"]);
    ingest(
        &server,
        weather_run("weather-1", "tr-parent", 0, "rain", json!({})),
        ACME_WRITER,
    );
    // Exactly the limit: more forks than it would mark the listing truncated
    // by themselves, and only the budget may do that here.
    let mut batch = Vec::new();
    for fork in 0..50 {
        batch.extend(recorded_fork(
            &format!("fork-{fork:02}"),
            &format!("tr-fork-{fork:02}"),
            10_000 + fork * 1_000,
            "sun",
            &format!("fk_{fork:02}"),
        ));
    }
    ingest(&server, batch, ACME_WRITER);
    let (status, body) = as_token(
        &server,
        "GET",
        "/v1/forks?run=weather-1&limit=50",
        None,
        ACME,
    );
    match status {
        // Refused at the tagged read or the parent's read...
        400 => assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("query-deadline-ms"),
            "{body}"
        ),
        // ...or listed, with every fork the budget did not reach undiffed
        // and the listing marked incomplete. Never a complete answer: fifty
        // forks cannot be read and diffed in a millisecond.
        200 => {
            assert_eq!(body["forks"].as_array().unwrap().len(), 50, "{body}");
            assert_eq!(body["truncated"], json!(true), "{body}");
            assert!(body["forks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|fork| fork["skipped"] == json!("deadline")));
        }
        other => panic!("unexpected {other}: {body}"),
    }
}

#[test]
fn an_annotation_walk_never_outlives_its_query_budget() {
    // Golden markers planted as the annotation log, which the server replays
    // as it starts: a population no test could append one fsync at a time,
    // and far more than a millisecond of walking.
    let dir = common::temp_dir();
    let mut log = String::new();
    for index in 0..300_000_u64 {
        log.push_str(
            &json!({
                "session_id": format!("s-{index}"), "name": "traza.golden", "value": true,
                "source": "human:test", "timestamp_ns": 1_000 + index,
            })
            .to_string(),
        );
        log.push('\n');
    }
    std::fs::write(dir.join("annotations.jsonl"), log).expect("plants the log");
    let server = Server::spawn_in(dir, &["--query-deadline-ms", "1"]);
    for path in [
        "/v1/annotations?name=traza.golden&limit=10",
        "/v1/goldens?limit=10",
    ] {
        let (status, _, body) = call(&server, "GET", path, None, &[]);
        assert_eq!(status, 400, "{path} is refused past its budget: {body}");
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("query-deadline-ms"),
            "{path}: {body}"
        );
    }
}
