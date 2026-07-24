//! Semantic-convention normalization for LLM/agent spans.
//!
//! Traza recognizes two overlapping vocabularies and folds them to one set of
//! facts for its derived views (sessions, cost/token analytics, the dashboard):
//!
//! - the **OpenLLMetry / OpenTelemetry GenAI** conventions emitted by
//!   Traceloop's `opentelemetry-semantic-conventions-ai` and OTel GenAI
//!   instrumentation — `gen_ai.*`, `llm.usage.*`, and `traceloop.*`;
//! - Traza's own native `llm.*` / `session.id` shorthand (docs/llm-semantics.md).
//!
//! Extraction is a pure function over a span's attribute map. The two
//! conventions are recognized together; a span carrying only the native
//! `llm.*` keys extracts exactly as it did before this module existed, so the
//! change is additive. When several recognized keys are present the precedence
//! tables below decide — most-specific / standard key first for model and
//! per-direction token counts, native `session.id` first for session identity.
//!
//! This module is the single source of truth for the key precedence; the
//! React dashboard mirrors it in `ui/src/lib/spans.js`.

use serde_json::{Map, Value};

// -- OpenTelemetry GenAI / OpenLLMetry (Traceloop) -------------------------
const GEN_AI_SYSTEM: &str = "gen_ai.system";
const GEN_AI_REQUEST_MODEL: &str = "gen_ai.request.model";
const GEN_AI_RESPONSE_MODEL: &str = "gen_ai.response.model";
const GEN_AI_USAGE_PROMPT_TOKENS: &str = "gen_ai.usage.prompt_tokens";
const GEN_AI_USAGE_INPUT_TOKENS: &str = "gen_ai.usage.input_tokens";
const GEN_AI_USAGE_COMPLETION_TOKENS: &str = "gen_ai.usage.completion_tokens";
const GEN_AI_USAGE_OUTPUT_TOKENS: &str = "gen_ai.usage.output_tokens";
const GEN_AI_USAGE_TOTAL_TOKENS: &str = "gen_ai.usage.total_tokens";
const GEN_AI_USAGE_COST: &str = "gen_ai.usage.cost";
const GEN_AI_CONVERSATION_ID: &str = "gen_ai.conversation.id";
const LLM_USAGE_TOTAL_TOKENS: &str = "llm.usage.total_tokens";
const LLM_REQUEST_TYPE: &str = "llm.request.type";
const TRACELOOP_SPAN_KIND: &str = "traceloop.span.kind";
const TRACELOOP_SESSION_ID: &str = "traceloop.association.properties.session_id";
const TRACELOOP_CHAT_ID: &str = "traceloop.association.properties.chat_id";

// -- Native Traza shorthand ------------------------------------------------
const LLM_MODEL: &str = "llm.model";
const LLM_PROMPT_TOKENS: &str = "llm.prompt_tokens";
const LLM_COMPLETION_TOKENS: &str = "llm.completion_tokens";
const LLM_TOTAL_TOKENS: &str = "llm.total_tokens";
const LLM_COST_USD: &str = "llm.cost_usd";
const SESSION_ID: &str = "session.id";

/// Attribute keys that identify a session, in resolution priority order:
/// native `session.id` first, then the OpenLLMetry / OTel GenAI conventions.
/// Ingest (rollups) and query (`Store::session`) share this list so a session
/// resolves the same way on both paths.
pub(crate) const SESSION_KEYS: [&str; 4] = [
    SESSION_ID,
    GEN_AI_CONVERSATION_ID,
    TRACELOOP_SESSION_ID,
    TRACELOOP_CHAT_ID,
];

/// Normalized LLM facts derived from a span's attributes. Both the OpenLLMetry
/// `gen_ai.*` conventions and Traza's native `llm.*` shorthand resolve here.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LlmFacts {
    /// Whether the span looks like an LLM/agent model call at all.
    pub is_llm: bool,
    /// Model identifier (response model preferred over request model).
    pub model: Option<String>,
    /// Provider/system (`gen_ai.system`, for example `openai`, `anthropic`).
    pub provider: Option<String>,
    /// Session identifier, if any recognized session key is present.
    pub session: Option<String>,
    /// The attribute key that supplied [`LlmFacts::session`].
    pub session_key: Option<&'static str>,
    /// Prompt/input token count, if reported.
    pub prompt_tokens: Option<u64>,
    /// Completion/output token count, if reported.
    pub completion_tokens: Option<u64>,
    /// Explicit total token count, if reported (see [`LlmFacts::total`]).
    pub total_tokens: Option<u64>,
    /// Metered cost in USD, if reported.
    pub cost_usd: Option<f64>,
}

impl LlmFacts {
    /// Resolved total tokens: the explicit total when present, else the
    /// saturating sum of prompt and completion.
    pub fn total(&self) -> u64 {
        self.total_tokens.unwrap_or_else(|| {
            self.prompt_tokens
                .unwrap_or(0)
                .saturating_add(self.completion_tokens.unwrap_or(0))
        })
    }
}

/// Extracts [`LlmFacts`] from a span's attribute map.
pub fn facts(attributes: &Map<String, Value>) -> LlmFacts {
    let model = first_str(
        attributes,
        &[GEN_AI_RESPONSE_MODEL, GEN_AI_REQUEST_MODEL, LLM_MODEL],
    );
    let provider = attr_str(attributes, GEN_AI_SYSTEM);
    let prompt_tokens = first_u64(
        attributes,
        &[
            GEN_AI_USAGE_PROMPT_TOKENS,
            GEN_AI_USAGE_INPUT_TOKENS,
            LLM_PROMPT_TOKENS,
        ],
    );
    let completion_tokens = first_u64(
        attributes,
        &[
            GEN_AI_USAGE_COMPLETION_TOKENS,
            GEN_AI_USAGE_OUTPUT_TOKENS,
            LLM_COMPLETION_TOKENS,
        ],
    );
    let total_tokens = first_u64(
        attributes,
        &[
            LLM_USAGE_TOTAL_TOKENS,
            GEN_AI_USAGE_TOTAL_TOKENS,
            LLM_TOTAL_TOKENS,
        ],
    );
    let cost_usd = first_f64(attributes, &[GEN_AI_USAGE_COST, LLM_COST_USD]);
    let (session, session_key) = SESSION_KEYS
        .iter()
        .find_map(|key| attr_str(attributes, key).map(|value| (value, *key)))
        .map_or((None, None), |(value, key)| (Some(value), Some(key)));
    let is_llm = model.is_some()
        || provider.is_some()
        || prompt_tokens.is_some()
        || completion_tokens.is_some()
        || total_tokens.is_some()
        || attributes.contains_key(LLM_REQUEST_TYPE)
        || attr_str(attributes, TRACELOOP_SPAN_KIND).as_deref() == Some("llm");
    LlmFacts {
        is_llm,
        model,
        provider,
        session,
        session_key,
        prompt_tokens,
        completion_tokens,
        total_tokens,
        cost_usd,
    }
}

fn first_str(attributes: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| attr_str(attributes, key))
}

fn first_u64(attributes: &Map<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| attr_u64(attributes, key))
}

fn first_f64(attributes: &Map<String, Value>, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| attr_f64(attributes, key))
}

/// String attribute, stringifying numbers; empty strings read as absent.
pub(crate) fn attr_str(attributes: &Map<String, Value>, key: &str) -> Option<String> {
    match attributes.get(key)? {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Numeric attribute, tolerant of numeric strings (native-JSON producers and
/// some OTLP exporters stringify counters).
pub(crate) fn attr_u64(attributes: &Map<String, Value>, key: &str) -> Option<u64> {
    match attributes.get(key)? {
        Value::Number(number) => number
            .as_u64()
            .or_else(|| number.as_f64().map(|value| value.max(0.0) as u64)),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

/// Floating attribute, tolerant of numeric strings; non-finite values drop.
pub(crate) fn attr_f64(attributes: &Map<String, Value>, key: &str) -> Option<f64> {
    let value = match attributes.get(key)? {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    };
    value.filter(|number| number.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn attrs(value: Value) -> Map<String, Value> {
        value.as_object().expect("object").clone()
    }

    #[test]
    fn native_shorthand_extracts_as_before() {
        let f = facts(&attrs(json!({
            "session.id": "sess",
            "llm.model": "m",
            "llm.prompt_tokens": 100,
            "llm.completion_tokens": 50,
            "llm.cost_usd": 0.01,
        })));
        assert!(f.is_llm);
        assert_eq!(f.model.as_deref(), Some("m"));
        assert_eq!(f.provider, None);
        assert_eq!(f.session.as_deref(), Some("sess"));
        assert_eq!(f.session_key, Some("session.id"));
        assert_eq!(f.prompt_tokens, Some(100));
        assert_eq!(f.completion_tokens, Some(50));
        assert_eq!(f.total(), 150, "no explicit total: prompt + completion");
        assert_eq!(f.cost_usd, Some(0.01));
    }

    #[test]
    fn openllmetry_gen_ai_conventions_are_recognized() {
        let f = facts(&attrs(json!({
            "gen_ai.system": "openai",
            "gen_ai.request.model": "gpt-4o",
            "gen_ai.usage.prompt_tokens": 120,
            "gen_ai.usage.completion_tokens": 80,
            "llm.usage.total_tokens": 200,
            "gen_ai.conversation.id": "chat-1",
        })));
        assert!(f.is_llm);
        assert_eq!(f.provider.as_deref(), Some("openai"));
        assert_eq!(f.model.as_deref(), Some("gpt-4o"));
        assert_eq!(f.prompt_tokens, Some(120));
        assert_eq!(f.completion_tokens, Some(80));
        assert_eq!(f.total_tokens, Some(200));
        assert_eq!(f.total(), 200);
        assert_eq!(f.session.as_deref(), Some("chat-1"));
        assert_eq!(f.session_key, Some("gen_ai.conversation.id"));
    }

    #[test]
    fn response_model_and_input_output_aliases_win_when_present() {
        let f = facts(&attrs(json!({
            "gen_ai.request.model": "req-model",
            "gen_ai.response.model": "resp-model",
            "gen_ai.usage.input_tokens": 7,
            "gen_ai.usage.output_tokens": 3,
        })));
        assert_eq!(
            f.model.as_deref(),
            Some("resp-model"),
            "response model preferred"
        );
        assert_eq!(f.prompt_tokens, Some(7));
        assert_eq!(f.completion_tokens, Some(3));
        assert_eq!(f.total(), 10);
    }

    #[test]
    fn numeric_strings_coerce() {
        let f = facts(&attrs(json!({
            "gen_ai.request.model": "m",
            "gen_ai.usage.prompt_tokens": "120",
            "gen_ai.usage.completion_tokens": "30",
            "gen_ai.usage.cost": "0.25",
        })));
        assert_eq!(f.prompt_tokens, Some(120));
        assert_eq!(f.completion_tokens, Some(30));
        assert_eq!(f.cost_usd, Some(0.25));
    }

    #[test]
    fn session_precedence_prefers_native_then_conversation_then_association() {
        let native = facts(&attrs(json!({
            "session.id": "native",
            "gen_ai.conversation.id": "conv",
        })));
        assert_eq!(native.session.as_deref(), Some("native"));
        assert_eq!(native.session_key, Some("session.id"));

        let association = facts(&attrs(json!({
            "traceloop.association.properties.chat_id": "chat-9",
        })));
        assert_eq!(association.session.as_deref(), Some("chat-9"));
        assert_eq!(
            association.session_key,
            Some("traceloop.association.properties.chat_id")
        );
    }

    #[test]
    fn provider_or_span_kind_alone_marks_an_llm_span() {
        assert!(facts(&attrs(json!({ "gen_ai.system": "anthropic" }))).is_llm);
        assert!(facts(&attrs(json!({ "traceloop.span.kind": "llm" }))).is_llm);
        assert!(facts(&attrs(json!({ "llm.request.type": "chat" }))).is_llm);
    }

    #[test]
    fn a_plain_span_is_not_an_llm_call() {
        let f = facts(&attrs(json!({ "http.method": "GET", "session.id": "s" })));
        assert!(!f.is_llm, "session alone is not an LLM call");
        assert_eq!(f.model, None);
        assert_eq!(f.total(), 0);
        assert_eq!(
            f.session.as_deref(),
            Some("s"),
            "but the session still groups"
        );
    }
}
