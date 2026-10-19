//! Fork & Rewind, end to end against the real binary: a replay told to serve
//! an edited answer or tool result at one step, to hand the run to the real
//! provider after another, and to explain a divergence; then the forks a
//! harness recorded found again and diffed against their parent, and the
//! runs marked golden listed for a test runner — all under tenant binding.
//!
//! Runs are seeded the way OpenLLMetry's instrumentations record them
//! (indexed `gen_ai.prompt.{i}.*`), and the provider routes are driven with
//! raw HTTP shaped as the official SDKs send it. The mechanics are tested on
//! a small weather run; the last test walks `dr-0917`, the research run the
//! replay and fork guides print, and holds every output they show.

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
                "gen_ai.request.model": "gpt-6.1-sol",
                "gen_ai.response.model": "gpt-6.1-sol",
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
                "gen_ai.request.model": "gpt-6.1-sol",
                "gen_ai.response.model": "gpt-6.1-sol",
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
    json!({"model": "gpt-6.1-sol", "messages": [
        {"role": "system", "content": SYSTEM},
        {"role": "user", "content": USER},
    ]})
}

fn second_request(tool_result: &str) -> Value {
    json!({"model": "gpt-6.1-sol", "messages": [
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
            &json!({"model": "claude-opus-5-5", "max_tokens": 64, "messages": [{"role": "user", "content": "hi"}]}),
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
        Some(&json!({"model": "claude-opus-5-5", "max_tokens": 64,
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

// ------------------------------------------------------------ the guides' research run

// `dr-0917`, the run the replay and fork guides (and the SDK READMEs) print,
// built span for span as the site's demo script records it: a coordinator on
// claude-opus-5-5 hands a web search to a web-researcher sub-agent on
// claude-haiku-5-5 through its `Agent` tool, then reads the sources with
// `WebFetch`; the sub-agent searches with `WebSearch`. The harness runs every
// tool itself, so each is a step. Prompts are OpenLLMetry's indexed layout,
// with the cache counters Anthropic reports. Every digest, fingerprint, quote
// and cost the guides print comes from these bytes: the test below is what
// keeps them true.

const RS_COORDINATOR: &str = "You are a research coordinator. Delegate web searches to the \
    web-researcher subagent, read the sources it finds with WebFetch, then write a one-page brief \
    that cites every source by URL.";
const RS_WORKER: &str = "You are web-researcher. Search the web and reply with the most relevant \
    primary sources, one line each, with their URLs.";
const RS_TASK: &str =
    "Write a one-page brief on solid-state battery readiness for EVs in 2027, with sources.";
const RS_BRIEF: &str =
    "Find 2026 primary sources on solid-state battery pilot production, yields and cost per kWh.";
const RS_QUERY: &str = "solid-state battery pilot line yield cost per kWh 2026";
const RS_FETCH_PROMPT: &str =
    "List the 2026 figures on pilot lines, yields, cost per kWh and sample cells.";
const RS_URLS: [&str; 3] = [
    "https://www.example-energy.org/2026/solid-state-pilot-lines",
    "https://www.example-batteries.com/reports/ssb-cost-2026",
    "https://www.example-auto.net/news/2026-ssb-sample-cells",
];
const RS_PAGES: [&str; 3] = [
    "Pilot lines (2026): three cell makers ran solid-state pilot lines at 0.2 to 1 GWh a year. \
     First-pass yields were reported between 60% and 75%.",
    "Cost (2026): solid-state cells were estimated at $180 to $240 per kWh, against about $95 for \
     LFP. The gap narrows only with sulfide electrolyte scale-up after 2028.",
    "Samples (2026): two automakers began road tests of B-sample solid-state cells late in the \
     year. Neither committed to a 2027 production vehicle.",
];
/// The size each page extract was reported at, in tokens.
const RS_PAGE_TOKENS: [u64; 3] = [6_200, 5_400, 4_800];
const RS_FINAL: &str = "Solid-state batteries in 2027: pilot scale, not production. Pilot lines \
    ran at 0.2 to 1 GWh a year in 2026 with 60 to 75% first-pass yields; cells cost $180 to $240 \
    per kWh against about $95 for LFP; automakers are road-testing B-samples but none has \
    committed to a 2027 vehicle. Expect limited premium launches after 2028.";
/// The list rates the guides' server was started with (`--pricing`).
const RS_PRICING: &str = r#"{"models": {
    "claude-opus-5-5": {"input_per_mtok": 4.0, "output_per_mtok": 20.0,
                        "cache_read_per_mtok": 0.2, "cache_write_per_mtok": 5.0},
    "claude-haiku-5-5": {"input_per_mtok": 0.1, "output_per_mtok": 0.5,
                         "cache_read_per_mtok": 0.01, "cache_write_per_mtok": 0.125}}}"#;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Who {
    Coordinator,
    Worker,
}

impl Who {
    fn model(self) -> &'static str {
        match self {
            Who::Coordinator => "claude-opus-5-5",
            Who::Worker => "claude-haiku-5-5",
        }
    }
    fn agent(self) -> &'static str {
        match self {
            Who::Coordinator => "research-coordinator",
            Who::Worker => "web-researcher",
        }
    }
    fn system(self) -> &'static str {
        match self {
            Who::Coordinator => RS_COORDINATOR,
            Who::Worker => RS_WORKER,
        }
    }
    /// The cached prefix: the agent's system prompt and tool definitions.
    fn prefix(self) -> u64 {
        match self {
            Who::Coordinator => 9_800,
            Who::Worker => 2_600,
        }
    }
}

/// A JSON object with its keys in the given order, as the harness's
/// `JSON.stringify` writes it; each value is already JSON.
fn rs_object(pairs: &[(&str, String)]) -> String {
    let fields: Vec<String> = pairs
        .iter()
        .map(|(key, value)| format!("{}:{value}", json!(key)))
        .collect();
    format!("{{{}}}", fields.join(","))
}

fn rs_str(text: &str) -> String {
    json!(text).to_string()
}

/// One step of the run as the harness lives it.
enum Plan {
    /// A model call answered with a tool call: the tool and its arguments, as written.
    Ask(Who, &'static str, String),
    /// A model call answered with text; `true` when it is the sub-agent's last.
    Say(Who, &'static str, bool),
    /// A tool the harness ran: name, result, its size in tokens, and whether
    /// the deduping harness answered it from what it already had.
    Run(&'static str, String, u64, bool),
}

/// The research run's plan: `fetches` are the pages the coordinator reads, in
/// order; `dedupe` is the harness that refuses to fetch a URL twice.
fn research_plan(fetches: &[usize], dedupe: bool) -> Vec<Plan> {
    let mut plan = vec![
        Plan::Ask(
            Who::Coordinator,
            "Agent",
            rs_object(&[
                ("description", rs_str("Battery sources")),
                ("prompt", rs_str(RS_BRIEF)),
                ("subagent_type", rs_str("web-researcher")),
            ]),
        ),
        Plan::Ask(
            Who::Worker,
            "WebSearch",
            rs_object(&[("query", rs_str(RS_QUERY))]),
        ),
    ];
    let search: Vec<String> = RS_URLS
        .iter()
        .enumerate()
        .map(|(index, url)| format!("{}. {url}", index + 1))
        .collect();
    plan.push(Plan::Run("WebSearch", search.join("\n"), 380, false));
    plan.push(Plan::Say(Who::Worker, "findings", true));
    let mut seen = Vec::new();
    for &page in fetches {
        plan.push(Plan::Ask(
            Who::Coordinator,
            "WebFetch",
            rs_object(&[
                ("url", rs_str(RS_URLS[page])),
                ("prompt", rs_str(RS_FETCH_PROMPT)),
            ]),
        ));
        if dedupe && seen.contains(&page) {
            let note = format!(
                "Already fetched {} earlier in this run; reusing that result.",
                RS_URLS[page]
            );
            plan.push(Plan::Run("WebFetch", note, 40, true));
        } else {
            plan.push(Plan::Run(
                "WebFetch",
                RS_PAGES[page].to_owned(),
                RS_PAGE_TOKENS[page],
                false,
            ));
        }
        seen.push(page);
    }
    plan.push(Plan::Say(Who::Coordinator, "brief", false));
    plan
}

fn rs_findings() -> String {
    let lines: Vec<String> = RS_URLS
        .iter()
        .zip(RS_PAGES)
        .map(|(url, page)| format!("- {url}: {}", page.split(':').next().unwrap()))
        .collect();
    format!("Sources:\n{}", lines.join("\n"))
}

/// A message as the harness sends it, and as its instrumentation records it.
#[derive(Clone)]
struct Message {
    role: &'static str,
    sent: Value,
    recorded: String,
}

impl Message {
    fn text(role: &'static str, text: &str) -> Message {
        Message {
            role,
            sent: json!({"role": role, "content": text}),
            recorded: text.to_owned(),
        }
    }
    fn blocks(role: &'static str, recorded: String) -> Message {
        let content: Value = serde_json::from_str(&recorded).expect("blocks are JSON");
        Message {
            role,
            sent: json!({"role": role, "content": content}),
            recorded,
        }
    }
}

enum Call {
    /// A model call: who made it, the messages it sent, its answer's content
    /// blocks as recorded, the tool call in them, and its usage (input,
    /// output, cache read, cache write).
    Model {
        who: Who,
        sent: Vec<Message>,
        content: String,
        tool: Option<(String, &'static str, String)>,
        text: Option<String>,
        usage: [u64; 4],
    },
    /// A tool the harness ran, for the tool call `id`.
    Tool {
        who: Who,
        name: &'static str,
        id: String,
        arguments: String,
        result: String,
        deduped: bool,
    },
    /// The sub-agent finished: its answer is the `Agent` call's result.
    Closes(String),
}

/// Walks a plan, building each agent's conversation exactly as the harness
/// sends it, with the usage the provider reports: the prompt read from the
/// cache up to the previous call, what was added since written to it.
fn research_calls(plan: &[Plan], key: &str) -> Vec<Call> {
    struct Convo {
        messages: Vec<Message>,
        ctx: u64,
        cached: Option<u64>,
    }
    let mut coordinator = Convo {
        messages: vec![Message::text("user", RS_TASK)],
        ctx: 40,
        cached: None,
    };
    let mut worker = Convo {
        messages: vec![Message::text("user", RS_BRIEF)],
        ctx: 30,
        cached: None,
    };
    let mut calls = Vec::new();
    let mut pending: Option<(Who, String, String)> = None;
    let mut task_id = String::new();
    let mut tools_asked = 0_u64;
    for step in plan {
        match step {
            Plan::Ask(who, _, _) | Plan::Say(who, _, _) => {
                let who = *who;
                let convo = if who == Who::Coordinator {
                    &mut coordinator
                } else {
                    &mut worker
                };
                let sent = convo.messages.clone();
                let read = convo.cached.map_or(0, |cached| who.prefix() + cached);
                let write = convo
                    .cached
                    .map_or(who.prefix() + convo.ctx, |cached| convo.ctx - cached);
                let (content, tool, text, output) = match step {
                    Plan::Ask(_, name, arguments) => {
                        tools_asked += 1;
                        let digest =
                            traza::payload::sha256_hex(format!("{key}:{tools_asked}").as_bytes());
                        let id = format!("toolu_{}", &digest[..22]);
                        let content = format!(
                            "[{}]",
                            rs_object(&[
                                ("type", rs_str("tool_use")),
                                ("id", rs_str(&id)),
                                ("name", rs_str(name)),
                                ("input", arguments.clone()),
                            ])
                        );
                        pending = Some((who, id.clone(), arguments.clone()));
                        if *name == "Agent" {
                            task_id = id.clone();
                        }
                        let output = 60 + (arguments.len() as u64).div_ceil(4);
                        (content, Some((id, *name, arguments.clone())), None, output)
                    }
                    Plan::Say(_, which, _) => {
                        let text = if *which == "brief" {
                            RS_FINAL.to_owned()
                        } else {
                            rs_findings()
                        };
                        let content = format!(
                            "[{}]",
                            rs_object(&[("type", rs_str("text")), ("text", rs_str(&text))])
                        );
                        let output = if who == Who::Coordinator { 1_900 } else { 720 };
                        (content, None, Some(text), output)
                    }
                    Plan::Run(..) => unreachable!(),
                };
                convo.cached = Some(convo.ctx);
                convo.ctx += output;
                convo
                    .messages
                    .push(Message::blocks("assistant", content.clone()));
                let usage = [3 + tools_asked % 5, output, read, write];
                let last = matches!(step, Plan::Say(_, _, true));
                calls.push(Call::Model {
                    who,
                    sent,
                    content,
                    tool,
                    text: text.clone(),
                    usage,
                });
                if last {
                    let text = text.expect("the sub-agent's answer");
                    let result = format!(
                        "[{}]",
                        rs_object(&[
                            ("type", rs_str("tool_result")),
                            ("tool_use_id", rs_str(&task_id)),
                            ("content", rs_str(&text)),
                        ])
                    );
                    coordinator.messages.push(Message::blocks("user", result));
                    coordinator.ctx += output;
                    calls.push(Call::Closes(text));
                }
            }
            Plan::Run(name, result, tokens, deduped) => {
                let (who, id, arguments) = pending.clone().expect("a tool call to answer");
                let convo = if who == Who::Coordinator {
                    &mut coordinator
                } else {
                    &mut worker
                };
                let block = format!(
                    "[{}]",
                    rs_object(&[
                        ("type", rs_str("tool_result")),
                        ("tool_use_id", rs_str(&id)),
                        ("content", rs_str(result)),
                    ])
                );
                convo.messages.push(Message::blocks("user", block));
                convo.ctx += tokens;
                calls.push(Call::Tool {
                    who,
                    name,
                    id,
                    arguments,
                    result: result.clone(),
                    deduped: *deduped,
                });
            }
        }
    }
    calls
}

/// The run's spans in Traza's native JSON: its root, each model call with the
/// prompt it sent and its answer, the `Agent` call holding the sub-agent, and
/// each tool call. `extra` lands on every span — a fork's tags.
fn research_spans(run: &str, key: &str, start: u64, calls: &[Call], extra: &Value) -> Vec<Value> {
    let trace = &traza::payload::sha256_hex(format!("trace:{key}").as_bytes())[..32];
    let mut serial = 0;
    let mut next_id = || {
        serial += 1;
        traza::payload::sha256_hex(format!("span:{key}:{serial}").as_bytes())[..16].to_owned()
    };
    let root = next_id();
    let base = |more: Value| {
        let mut attributes = json!({"session.id": run});
        for source in [extra, &more] {
            for (name, value) in source.as_object().expect("an object") {
                attributes[name] = value.clone();
            }
        }
        attributes
    };
    let span = |id: &str, parent: &str, name: String, start: u64, ms: u64, attributes: Value| {
        json!({
            "trace_id": trace, "span_id": id, "parent_span_id": parent, "name": name,
            "service": "research-coordinator", "status": "ok",
            "start_time_unix_nano": start, "end_time_unix_nano": start + ms * MS,
            "attributes": attributes,
        })
    };
    let mut spans: Vec<Value> = Vec::new();
    let mut clock = start + 5 * MS;
    let (mut task, mut sub_agent) = (usize::MAX, usize::MAX);
    for step in calls {
        match step {
            Call::Closes(result) => {
                spans[sub_agent]["end_time_unix_nano"] = json!(clock);
                spans[task]["end_time_unix_nano"] = json!(clock + 2 * MS);
                spans[task]["attributes"]["gen_ai.tool.call.result"] = json!(result);
                clock += 10 * MS;
            }
            Call::Model {
                who,
                sent,
                content: _,
                tool,
                text,
                usage,
            } => {
                let model = who.model();
                let ms = match (who, text.is_some()) {
                    (Who::Coordinator, true) => 21_400,
                    (Who::Coordinator, false) => 3_600,
                    (Who::Worker, true) => 4_100,
                    (Who::Worker, false) => 1_200,
                };
                let mut attributes = base(json!({
                    "gen_ai.operation.name": "chat", "gen_ai.system": "anthropic",
                    "gen_ai.provider.name": "anthropic",
                    "gen_ai.request.model": model, "gen_ai.response.model": model,
                    "gen_ai.agent.name": who.agent(),
                    "gen_ai.usage.input_tokens": usage[0], "gen_ai.usage.output_tokens": usage[1],
                    "gen_ai.usage.cache_read.input_tokens": usage[2],
                    "gen_ai.usage.cache_creation.input_tokens": usage[3],
                    "gen_ai.prompt.0.role": "system", "gen_ai.prompt.0.content": who.system(),
                    "gen_ai.completion.0.role": "assistant",
                }));
                for (index, message) in sent.iter().enumerate() {
                    attributes[format!("gen_ai.prompt.{}.role", index + 1)] = json!(message.role);
                    attributes[format!("gen_ai.prompt.{}.content", index + 1)] =
                        json!(message.recorded);
                }
                match (text, tool) {
                    (Some(text), _) => {
                        attributes["gen_ai.completion.0.content"] = json!(text);
                        attributes["gen_ai.completion.0.finish_reason"] = json!("end_turn");
                    }
                    (None, Some((id, name, arguments))) => {
                        attributes["gen_ai.completion.0.finish_reason"] = json!("tool_use");
                        attributes["gen_ai.completion.0.tool_calls.0.id"] = json!(id);
                        attributes["gen_ai.completion.0.tool_calls.0.name"] = json!(name);
                        attributes["gen_ai.completion.0.tool_calls.0.arguments"] = json!(arguments);
                    }
                    (None, None) => unreachable!(),
                }
                let parent = if *who == Who::Worker {
                    spans[sub_agent]["span_id"].as_str().unwrap().to_owned()
                } else {
                    root.clone()
                };
                let id = next_id();
                spans.push(span(
                    &id,
                    &parent,
                    format!("chat {model}"),
                    clock,
                    ms,
                    attributes,
                ));
                clock += (ms + 40) * MS;
                if let Some((id, "Agent", arguments)) = tool {
                    let task_id = next_id();
                    spans.push(span(
                        &task_id,
                        &root,
                        "execute_tool Agent".into(),
                        clock,
                        0,
                        base(json!({
                            "gen_ai.operation.name": "execute_tool", "gen_ai.tool.name": "Agent",
                            "gen_ai.agent.name": Who::Coordinator.agent(),
                            "gen_ai.tool.call.id": id, "gen_ai.tool.call.arguments": arguments,
                        })),
                    ));
                    task = spans.len() - 1;
                    clock += 5 * MS;
                    let agent_id = next_id();
                    spans.push(span(
                        &agent_id,
                        &task_id,
                        "invoke_agent web-researcher".into(),
                        clock,
                        0,
                        base(json!({
                            "gen_ai.operation.name": "invoke_agent",
                            "gen_ai.agent.name": Who::Worker.agent(),
                        })),
                    ));
                    sub_agent = spans.len() - 1;
                    clock += 5 * MS;
                }
            }
            Call::Tool {
                who,
                name,
                id,
                arguments,
                result,
                deduped,
            } => {
                let ms = match (*name, deduped) {
                    ("WebSearch", _) => 900,
                    (_, true) => 2,
                    _ => 1_700,
                };
                let parent = if *who == Who::Worker {
                    spans[sub_agent]["span_id"].as_str().unwrap().to_owned()
                } else {
                    root.clone()
                };
                let span_id = next_id();
                spans.push(span(
                    &span_id,
                    &parent,
                    format!("execute_tool {name}"),
                    clock,
                    ms,
                    base(json!({
                        "gen_ai.operation.name": "execute_tool", "gen_ai.tool.name": name,
                        "gen_ai.agent.name": who.agent(), "gen_ai.tool.call.id": id,
                        "gen_ai.tool.call.arguments": arguments, "gen_ai.tool.call.result": result,
                    })),
                ));
                clock += (ms + 40) * MS;
            }
        }
    }
    let mut root_span = span(
        &root,
        "",
        "invoke_agent research-coordinator".into(),
        start,
        (clock + 10 * MS - start) / MS,
        base(json!({
            "gen_ai.operation.name": "invoke_agent",
            "gen_ai.agent.name": Who::Coordinator.agent(),
        })),
    );
    root_span["parent_span_id"] = Value::Null;
    spans.insert(0, root_span);
    spans
}

/// How a walk of a replay ended: every call answered, or refused at a call.
enum Walk {
    Done {
        /// Each `tools/next` answer, in order.
        tools: Vec<Value>,
        /// The messages of each model request, in order.
        requests: Vec<Value>,
    },
    Refused {
        tools: Vec<Value>,
        /// The last is the request refused, when a model call was.
        requests: Vec<Value>,
        status: u16,
        head: Vec<(String, String)>,
        body: Value,
    },
}

/// The messages of each model request a run recorded, in order.
fn recorded_requests(calls: &[Call]) -> Vec<Value> {
    calls
        .iter()
        .filter_map(|step| match step {
            Call::Model { sent, .. } => Some(Value::Array(
                sent.iter().map(|message| message.sent.clone()).collect(),
            )),
            _ => None,
        })
        .collect()
}

/// The research harness against a replay. `calls` says only what it does
/// next: which agent calls its model, which tool it runs. Each conversation
/// is built from what the replay answers: the served answer, and each tool
/// result from `tools/next` — except a URL the deduping harness refuses to
/// fetch again, which it answers itself. The sub-agent starts from the brief
/// in the coordinator's `Agent` call, and its last answer is that call's
/// result. Model calls go over the Anthropic route as the Anthropic SDK sends
/// them (`x-api-key`); a tool result that is not text goes back as JSON.
fn research_walk(server: &Server, replay: &str, calls: &[Call]) -> Walk {
    let mut coordinator = vec![json!({"role": "user", "content": RS_TASK})];
    let mut worker: Vec<Value> = Vec::new();
    let mut tools = Vec::new();
    let mut requests = Vec::new();
    for step in calls {
        match step {
            Call::Model { who, content, .. } => {
                let convo = if *who == Who::Coordinator {
                    &mut coordinator
                } else {
                    &mut worker
                };
                let messages = Value::Array(convo.clone());
                requests.push(messages.clone());
                let (status, head, body) = call_anthropic(
                    server,
                    replay,
                    &json!({"model": who.model(), "max_tokens": 8192, "system": who.system(),
                            "messages": messages}),
                );
                if status != 200 {
                    return Walk::Refused {
                        tools,
                        requests,
                        status,
                        head,
                        body,
                    };
                }
                let recorded: Value = serde_json::from_str(content).unwrap();
                assert_eq!(body["content"], recorded, "the recorded answer is served");
                let brief = body["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|block| block["type"] == "tool_use" && block["name"] == "Agent")
                    .map(|block| block["input"]["prompt"].clone());
                convo.push(json!({"role": "assistant", "content": body["content"]}));
                if let Some(brief) = brief {
                    worker = vec![json!({"role": "user", "content": brief})];
                }
            }
            Call::Tool {
                who,
                name,
                result,
                deduped,
                ..
            } => {
                let answer = if *deduped {
                    result.clone()
                } else {
                    let (status, head, body) = call(
                        server,
                        "POST",
                        &format!("/v1/replays/{replay}/tools/next"),
                        Some(&json!({"tool": name})),
                        &[bearer(ACME)],
                    );
                    if status != 200 {
                        return Walk::Refused {
                            tools,
                            requests,
                            status,
                            head,
                            body,
                        };
                    }
                    let answer = match &body["result"] {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    };
                    tools.push(body);
                    answer
                };
                let convo = if *who == Who::Coordinator {
                    &mut coordinator
                } else {
                    &mut worker
                };
                rs_answer_tool(convo, answer);
            }
            Call::Closes(_) => {
                let findings = worker
                    .last()
                    .and_then(|answer| answer["content"].as_array())
                    .and_then(|blocks| blocks.iter().find(|block| block["type"] == "text"))
                    .and_then(|block| block["text"].as_str())
                    .expect("the sub-agent's last answer is text")
                    .to_owned();
                rs_answer_tool(&mut coordinator, findings);
            }
        }
    }
    Walk::Done { tools, requests }
}

/// Answers the tool call in an agent's last answer with `result`.
fn rs_answer_tool(convo: &mut Vec<Value>, result: String) {
    let id = convo
        .last()
        .and_then(|answer| answer["content"].as_array())
        .and_then(|blocks| blocks.iter().find(|block| block["type"] == "tool_use"))
        .expect("a tool call to answer")["id"]
        .clone();
    convo.push(json!({"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": id, "content": result},
    ]}));
}

fn call_anthropic(
    server: &Server,
    replay: &str,
    body: &Value,
) -> (u16, Vec<(String, String)>, Value) {
    call(
        server,
        "POST",
        &format!("/v1/replays/{replay}/anthropic/v1/messages"),
        Some(body),
        &[
            ("x-api-key", ACME.to_owned()),
            ("anthropic-version", "2023-06-01".to_owned()),
        ],
    )
}

fn create_replay(server: &Server, body: Value) -> Value {
    let (status, created) = as_token(server, "POST", "/v1/replays", Some(&body), ACME);
    assert_eq!(status, 201, "{body} -> {created}");
    created
}

fn close_to(value: &Value, expected: f64) -> bool {
    value
        .as_f64()
        .is_some_and(|got| (got - expected).abs() < 1e-9)
}

#[test]
fn the_guides_research_run_replays_refuses_and_forks_as_they_print() {
    let dir = common::temp_dir();
    let pricing = dir.join("pricing.json");
    std::fs::write(&pricing, RS_PRICING).expect("writes the rates");
    let server = server(&["--pricing", pricing.to_str().expect("utf8")]);
    // The original harness read the pilot-lines page and the cost page twice.
    let fetches = [0, 1, 0, 2, 1];
    let recorded = research_calls(&research_plan(&fetches, false), "dr-0917");
    ingest(
        &server,
        research_spans("dr-0917", "dr-0917", T0, &recorded, &json!({})),
        ACME_WRITER,
    );

    // The tape (replay guide, "Runs and tapes"): eighteen steps, every span one.
    let (status, tape) = as_token(&server, "GET", "/v1/tapes/dr-0917", None, ACME);
    assert_eq!(status, 200, "{tape}");
    assert_eq!(tape["spans_examined"], json!(18));
    let steps = tape["steps"].as_array().unwrap();
    let fingerprints: Vec<&str> = steps
        .iter()
        .map(|step| step["fingerprint"].as_str().unwrap())
        .collect();
    assert_eq!(
        fingerprints[..6],
        [
            "agent:research-coordinator",
            "llm:claude-opus-5-5#ea97869dbfadddfc",
            "tool:Agent#b8a25c5ef91fd1e8",
            "agent:web-researcher",
            "llm:claude-haiku-5-5#629f8c5aa2e5db83",
            "tool:WebSearch#77934dd2a4e0f5da",
        ]
    );
    assert_eq!(
        steps[1]["request_digest"],
        json!("ea97869dbfadddfc08be3a151877b8a2e29b6dfb7f39f1fe43e2621484244b50")
    );
    assert_eq!(
        steps[1]["response_digest"],
        json!("54d13d575a267c22e2ea90d26527de3fed2cca599910b5fa55da7bea41db5818")
    );
    assert_eq!(
        steps[1]["tokens"],
        json!({"input": 4, "output": 103, "cache_read": 0, "cache_write": 9840, "context": 9844})
    );
    assert!(close_to(&steps[1]["cost_usd"], 0.051276), "{}", steps[1]);

    // A strict replay, walked by the recorded harness: nine model calls, the
    // sub-agent's two among them, every one matched.
    let created = create_replay(&server, json!({"run": "dr-0917"}));
    assert_eq!(
        (created["steps"].clone(), created["llm_steps"].clone()),
        (json!(18), json!(9))
    );
    let replay = created["id"].as_str().unwrap().to_owned();
    let Walk::Done { tools, requests } = research_walk(&server, &replay, &recorded) else {
        panic!("the recorded harness was refused");
    };
    assert_eq!(
        requests,
        recorded_requests(&recorded),
        "with nothing edited, the harness sends exactly what it recorded"
    );
    assert_eq!(tools[1]["step"], json!(8), "{}", tools[1]);
    assert_eq!(tools[1]["result"], json!(RS_PAGES[0]));
    let (_, report) = as_token(&server, "GET", &format!("/v1/replays/{replay}"), None, ACME);
    assert_eq!(
        (report["served"].clone(), report["remaining"].clone()),
        (json!(9), json!(0))
    );
    assert_eq!(report["diverged"], Value::Null);
    let served: Vec<(u64, bool)> = report["calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|call| {
            (
                call["step"].as_u64().unwrap(),
                call["matched"] == json!(true),
            )
        })
        .collect();
    assert_eq!(
        served,
        [1, 4, 6, 7, 9, 11, 13, 15, 17].map(|step| (step, true))
    );

    // The harness that refuses to fetch a URL twice, replayed strictly: its
    // call after the refused fetch is the first that differs (replay guide,
    // "Strict and loose matching"; fork guide, "Tape tests").
    let deduped = research_calls(&research_plan(&fetches, true), "dr-0917");
    let strict = create_replay(&server, json!({"run": "dr-0917"}));
    let strict = strict["id"].as_str().unwrap();
    let Walk::Refused {
        requests,
        status,
        head,
        body,
        ..
    } = research_walk(&server, strict, &deduped)
    else {
        panic!("the deduping harness walked a strict replay");
    };
    assert_eq!(
        requests[..],
        recorded_requests(&deduped)[..7],
        "the deduping harness sends its own conversation, refused at its seventh call"
    );
    assert_eq!(status, 409, "{body}");
    assert_eq!(header(&head, "x-should-retry"), Some("false"));
    assert_eq!(body["type"], json!("error"));
    let error = &body["error"];
    assert_eq!(error["type"], json!("replay_divergence"));
    assert_eq!(error["step"], json!(13));
    assert_eq!(
        error["expected_digest"],
        json!("38d64ab342a7ff12be1b446cc0209eecabbae76f5d4ec73b59a9e88a178ddd6d")
    );
    assert_eq!(
        error["got_digest"],
        json!("2d02f0080506df8f483cde7735ec2240ee801fdbd82f82836201aa8a882f0cb1")
    );
    assert_eq!(
        error["difference"],
        json!({
            "message": 9, "role": "user", "field": "content", "at_char": 13,
            "recorded": "[{\"content\":\"Pilot lines (2026): three cell makers ran solid-state pilot lines at 0.2 to 1 GWh a year. First-pass yields…",
            "got": "[{\"content\":\"Already fetched https://www.example-energy.org/2026/solid-state-pilot-lines earlier in this run; reusing th…",
            "recorded_messages": 10, "got_messages": 10,
        })
    );
    let (_, report) = as_token(&server, "GET", &format!("/v1/replays/{strict}"), None, ACME);
    assert_eq!(report["served"], json!(6));
    assert_eq!(report["diverged"]["difference"], error["difference"]);

    // The fork guide's thirty seconds: the cost page gone at step 10, live after it.
    let fork = create_replay(
        &server,
        json!({"run": "dr-0917",
               "edits": [{"step": 10, "tool_result": {"error": "fetch failed: 404"}}],
               "live_after": 10}),
    );
    assert_eq!(
        fork["llm_steps"],
        json!(5),
        "nothing past the live point is held"
    );
    let fork_id = fork["fork_id"].as_str().unwrap().to_owned();
    assert_eq!(
        fork["fork"],
        json!({"id": fork_id, "parent": "dr-0917", "step": 10,
               "edits": [{"step": 10, "kind": "tool_result"}], "live_after": 10,
               "attributes": {"traza.fork.parent": "dr-0917", "traza.fork.step": 10,
                              "traza.fork.id": fork_id}})
    );
    let Walk::Refused {
        tools,
        requests,
        status,
        head,
        body,
    } = research_walk(&server, fork["id"].as_str().unwrap(), &recorded)
    else {
        panic!("the fork never went live");
    };
    assert_eq!(
        tools[2],
        json!({"step": 10, "tool": "WebFetch",
               "arguments": {"prompt": RS_FETCH_PROMPT, "url": RS_URLS[1]},
               "result": {"error": "fetch failed: 404"}, "edited": true})
    );
    assert_eq!(status, 409, "{body}");
    assert_eq!(header(&head, "x-should-retry"), Some("false"));
    assert_eq!(
        (
            body["error"]["type"].clone(),
            body["error"]["live_after"].clone(),
            body["error"]["served"].clone(),
            body["error"]["fork_id"].clone()
        ),
        (json!("replay_live"), json!(10), json!(5), json!(fork_id))
    );
    // The call refused is step 11, and it carries the failed fetch: the
    // recorded conversation with the cost page swapped for the 404. That is
    // the request the harness takes to the real provider.
    let parent_requests = recorded_requests(&recorded);
    assert_eq!(requests.len(), 6, "five served, the sixth refused");
    assert_eq!(requests[..5], parent_requests[..5]);
    let mut with_404 = parent_requests[5].clone();
    let fetched = &mut with_404.as_array_mut().unwrap().last_mut().unwrap()["content"][0];
    assert_eq!(
        (fetched["type"].clone(), fetched["content"].clone()),
        (json!("tool_result"), json!(RS_PAGES[1])),
        "the recording read the cost page"
    );
    fetched["content"] = json!(r#"{"error":"fetch failed: 404"}"#);
    assert_eq!(requests[5], with_404);

    // A model answer cannot stand in for a tool step.
    let (status, refused) = as_token(
        &server,
        "POST",
        "/v1/replays",
        Some(&json!({"run": "dr-0917", "edits": [{"step": 10, "response": "The page is gone."}]})),
        ACME,
    );
    assert_eq!(status, 400);
    assert_eq!(
        refused,
        json!({"error": "step 10 is a tool call (tool:WebFetch), so a \"response\" edit cannot replace it: a tool result is edited with \"tool_result\""})
    );

    // The rewind the deduping harness was tried with: replayed through step
    // 11, live from its cut at 12, where the harness refuses the fetch.
    let rewind = create_replay(&server, json!({"run": "dr-0917", "live_after": 11}));
    assert_eq!(rewind["fork"]["step"], json!(12));
    let Walk::Refused {
        requests,
        status,
        body,
        ..
    } = research_walk(&server, rewind["id"].as_str().unwrap(), &deduped)
    else {
        panic!("the rewind never went live");
    };
    assert_eq!(requests[..], recorded_requests(&deduped)[..7]);
    assert_eq!(status, 409);
    assert_eq!(
        (
            body["error"]["type"].clone(),
            body["error"]["served"].clone()
        ),
        (json!("replay_live"), json!(6))
    );
    // Its live half, as the harness's instrumentation recorded it: the whole
    // run under its own session id, every span tagged with the fork.
    let live = research_calls(&research_plan(&[0, 1, 0, 2], true), "dr-0917");
    ingest(
        &server,
        research_spans(
            "dr-0917-dedupe",
            "dr-0917-dedupe",
            T0 + 90 * 60_000 * MS,
            &live,
            &rewind["fork"]["attributes"],
        ),
        ACME_WRITER,
    );

    // The fork listing and the diff the guides print.
    let explanation = "step 12 of a and step 12 of b (tool:WebFetch) received the same input and \
                       produced a different output: response e450b5a73db1 in a, 750af04196bb in b";
    let (status, listing) = as_token(&server, "GET", "/v1/forks?run=dr-0917", None, ACME);
    assert_eq!(status, 200, "{listing}");
    let parent = &listing["parent"];
    assert_eq!(
        (
            parent["steps"].clone(),
            parent["tokens"].clone(),
            parent["outcome"].clone()
        ),
        (json!(18), json!(3341), json!("success"))
    );
    assert!(close_to(&parent["cost_usd"], 0.271217875), "{parent}");
    let forks = listing["forks"].as_array().unwrap();
    assert_eq!(forks.len(), 1, "{listing}");
    let dedupe = &forks[0];
    assert_eq!(dedupe["run"], json!("dr-0917-dedupe"));
    assert_eq!(dedupe["fork_id"], rewind["fork_id"]);
    assert_eq!(dedupe["fork_step"], json!(12));
    assert_eq!(
        (
            dedupe["steps"].clone(),
            dedupe["outcome"].clone(),
            dedupe["diverged_before_cut"].clone()
        ),
        (json!(16), json!("success"), json!(false))
    );
    assert!(close_to(&dedupe["cost_usd"], 0.202954675), "{dedupe}");
    assert_eq!(
        dedupe["first_divergence"],
        json!({"a": 12, "b": 12, "kind": "decision", "explanation": explanation})
    );
    assert_eq!(
        (
            dedupe["delta"]["steps"].clone(),
            dedupe["delta"]["tokens"].clone(),
            dedupe["delta"]["duration_ns"].clone()
        ),
        (json!(-2), json!(-105), json!(-7_078_000_000_i64))
    );
    assert!(
        close_to(&dedupe["delta"]["cost_usd"], -0.0682632),
        "{dedupe}"
    );
    let (status, diff) = as_token(
        &server,
        "GET",
        "/v1/diff?a=dr-0917&b=dr-0917-dedupe",
        None,
        ACME,
    );
    assert_eq!(status, 200, "{diff}");
    assert_eq!(diff["first_divergence"], dedupe["first_divergence"]);
    let ops: Vec<String> = diff["aligned"]
        .as_array()
        .unwrap()
        .iter()
        .map(|pair| {
            format!(
                "{}{}",
                pair["op"].as_str().unwrap(),
                pair["why"]
                    .as_str()
                    .map(|why| format!(":{why}"))
                    .unwrap_or_default()
            )
        })
        .collect();
    let mut expected = vec!["same".to_owned(); 12];
    expected.extend(
        [
            "changed:decision",
            "changed:input",
            "same",
            "changed:input",
            "delete:structure",
            "delete:structure",
        ]
        .map(str::to_owned),
    );
    assert_eq!(ops, expected);
}
