//! Configuration fingerprints: what a model call was CONFIGURED with, read
//! off the span that recorded it. Computed on every read, never stored.
//!
//! | Dimension | Read from (first present wins) | Identity |
//! |---|---|---|
//! | `system_prompt` | the request's `system` / `developer` messages, `gen_ai.system_instructions` folded in ([`semconv::step_facts`] request precedence); for a request that is a provider body (OpenInference `input.value`), its top-level `system` and its `messages` | SHA-256 of the text with volatile tokens masked, 16 hex |
//! | `tool_catalog` | `gen_ai.tool.definitions` → indexed `llm.request.functions.{i}.*` → indexed `llm.tools.{i}.tool.json_schema` → a provider request body's `tools` | SHA-256 of the definitions' canonical JSON, sorted, 16 hex |
//! | `model_requested` | `gen_ai.request.model` → `llm.model` → `llm.model_name` | the value |
//! | `model_returned` | `gen_ai.response.model` | the value |
//! | `params` | every `gen_ai.request.*` key but the model, and `llm.invocation_parameters` (its `model` key dropped) — the keys a run diff calls `config` | `key=value` pairs in key order |
//! | `service_version` | `service.version` (any span) | the value |
//! | `sdk_version` | `telemetry.sdk.name` / `telemetry.sdk.version` (any span) | `name/version` |
//!
//! A value longer than [`MAX_VALUE_CHARS`] is identified by its digest and
//! displayed cut short. A request with no system message has the system
//! prompt `(none)` — removing a system prompt is a change too — while a
//! request that was not captured, or was offloaded whole to the payload
//! store, has no system-prompt value at all: it cannot be read without a
//! payload read per step, and guessing would be worse than abstaining.
//!
//! **Volatile tokens.** System prompts routinely embed today's date, a
//! request id or a timestamp, which would make every run's prompt unique and
//! hide the edit someone actually made. Before digesting, these are masked
//! as `<v>`: UUIDs, ISO dates (with an optional time), clock times, runs of
//! six or more digits, and runs of sixteen or more hex characters — each
//! only as a whole token. Short numbers survive on purpose, so `policy v2` →
//! `policy v3` is still a change.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::Dimension;
use crate::semconv::{self, attr_str};
use crate::Span;

/// Characters of a value kept as its identity and display before it is
/// identified by digest instead.
pub(crate) const MAX_VALUE_CHARS: usize = 160;

/// Bytes of a system prompt kept (masked) as its sample, for the change's
/// text diff.
pub(crate) const SAMPLE_BYTES: usize = 4096;

/// Tool names kept per catalog sample.
pub(crate) const MAX_CATALOG_NAMES: usize = 64;

/// The display of a configured absence: a request with no system message, an
/// empty tool list.
pub(crate) const NONE: &str = "(none)";

const SERVICE_VERSION: &str = "service.version";
const SDK_NAME: &str = "telemetry.sdk.name";
const SDK_VERSION: &str = "telemetry.sdk.version";
const ENVIRONMENT_NAME: &str = "deployment.environment.name";
const ENVIRONMENT_LEGACY: &str = "deployment.environment";
const GEN_AI_TOOL_DEFINITIONS: &str = "gen_ai.tool.definitions";
const LLM_REQUEST_FUNCTIONS: &str = "llm.request.functions.";
const OI_TOOLS: &str = "llm.tools.";
const OI_TOOL_SCHEMA: &str = "tool.json_schema";

/// What a change's detail can show beyond the two identities.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Sample {
    /// A system prompt, volatile tokens masked, cut to [`SAMPLE_BYTES`].
    Text(String),
    /// A tool catalog's tool names, sorted, at most [`MAX_CATALOG_NAMES`].
    Names(Vec<String>),
    /// Sampling parameters, key → compact value.
    Params(BTreeMap<String, String>),
}

/// One dimension's value for one step.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Fingerprint {
    /// What two steps compare equal on.
    pub(crate) key: String,
    /// What a reader sees: the value, or a digest.
    pub(crate) display: String,
    /// What a change's detail can diff.
    pub(crate) sample: Option<Sample>,
}

impl Fingerprint {
    /// A plain value: its own identity, or its digest when it is long.
    pub(crate) fn value(text: &str) -> Self {
        if text.chars().count() <= MAX_VALUE_CHARS {
            return Self {
                key: text.to_owned(),
                display: text.to_owned(),
                sample: None,
            };
        }
        let mut display: String = text.chars().take(MAX_VALUE_CHARS).collect();
        display.push('…');
        Self {
            key: format!("#{}", digest16(text.as_bytes())),
            display,
            sample: None,
        }
    }

    fn none() -> Self {
        Self::value(NONE)
    }
}

/// The first 16 hex characters of a SHA-256: 64 bits, so two different
/// configurations colliding is not a practical concern.
pub(crate) fn digest16(bytes: &[u8]) -> String {
    let mut hex = crate::payload::sha256_hex(bytes);
    hex.truncate(16);
    hex
}

/// Every configuration dimension one llm step carries.
pub(crate) fn llm_config(span: &Span) -> Vec<(Dimension, Fingerprint)> {
    let attributes = &span.attributes;
    // Read once: the request may be a long message list or a request body,
    // and both the system prompt and the catalog fallback need it.
    let request = semconv::llm_request_of(span);
    let mut found = Vec::with_capacity(5);
    if let Some(prompt) = request.as_ref().and_then(system_prompt) {
        found.push((Dimension::SystemPrompt, prompt));
    }
    if let Some(catalog) = tool_catalog(attributes, request.as_ref()) {
        found.push((Dimension::ToolCatalog, catalog));
    }
    if let Some(model) = model_requested(attributes) {
        found.push((Dimension::ModelRequested, Fingerprint::value(&model)));
    }
    if let Some(model) = attr_str(attributes, "gen_ai.response.model") {
        found.push((Dimension::ModelReturned, Fingerprint::value(&model)));
    }
    if let Some(params) = params(attributes) {
        found.push((Dimension::Params, params));
    }
    found
}

/// The deployed service version, from any span.
pub(crate) fn service_version(attributes: &Map<String, Value>) -> Option<Fingerprint> {
    attr_str(attributes, SERVICE_VERSION).map(|version| Fingerprint::value(&version))
}

/// The telemetry SDK and its version, from any span.
pub(crate) fn sdk_version(attributes: &Map<String, Value>) -> Option<Fingerprint> {
    let text = match (
        attr_str(attributes, SDK_NAME),
        attr_str(attributes, SDK_VERSION),
    ) {
        (Some(name), Some(version)) => format!("{name}/{version}"),
        (Some(name), None) => name,
        (None, Some(version)) => version,
        (None, None) => return None,
    };
    Some(Fingerprint::value(&text))
}

/// The deployment environment: `deployment.environment.name`, else the
/// older `deployment.environment`.
pub(crate) fn environment(attributes: &Map<String, Value>) -> Option<String> {
    attr_str(attributes, ENVIRONMENT_NAME).or_else(|| attr_str(attributes, ENVIRONMENT_LEGACY))
}

fn model_requested(attributes: &Map<String, Value>) -> Option<String> {
    ["gen_ai.request.model", "llm.model", "llm.model_name"]
        .iter()
        .find_map(|key| attr_str(attributes, key))
}

/// The sampling parameters, as `key=value` pairs in key order.
fn params(attributes: &Map<String, Value>) -> Option<Fingerprint> {
    let mut pairs: BTreeMap<String, String> = BTreeMap::new();
    for (key, value) in attributes {
        if let Some(name) = key.strip_prefix(crate::tape::CONFIG_PREFIX) {
            if name != "model" && !value.is_null() {
                pairs.insert(name.to_owned(), compact(value));
            }
        }
    }
    if let Some(blob) = attributes.get(crate::tape::OI_INVOCATION_PARAMETERS) {
        match semconv::field_value(blob) {
            Value::Object(map) => {
                for (name, value) in map {
                    if name != "model" && !value.is_null() {
                        pairs.entry(name).or_insert_with(|| compact(&value));
                    }
                }
            }
            Value::Null => {}
            other => {
                pairs
                    .entry("invocation_parameters".to_owned())
                    .or_insert_with(|| compact(&other));
            }
        }
    }
    if pairs.is_empty() {
        return None;
    }
    let text = pairs
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut fingerprint = Fingerprint::value(&text);
    fingerprint.sample = Some(Sample::Params(
        pairs
            .into_iter()
            .map(|(key, value)| (key, clip(&value, MAX_VALUE_CHARS)))
            .collect(),
    ));
    Some(fingerprint)
}

/// A parameter value as text: a string as itself, anything else as its
/// canonical JSON.
fn compact(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => crate::evals::canonical_json(other),
    }
}

/// `text` cut to `chars` characters, with an ellipsis when it was cut.
pub(crate) fn clip(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(chars).collect();
    cut.push('…');
    cut
}

/// The system prompt an llm step was given, per the module table.
fn system_prompt(request: &Value) -> Option<Fingerprint> {
    let mut text = String::new();
    let mut references = String::new();
    let mut found = false;
    let empty = Vec::new();
    let messages = match request {
        Value::Array(messages) => messages,
        // Offloaded whole and not read: unknown, not absent.
        Value::Object(object) if object.contains_key(crate::payload::PAYLOAD_KEY) => return None,
        // A provider request body (OpenInference's `input.value`): its
        // top-level `system` (Anthropic's shape) and its `messages`.
        Value::Object(object) => {
            if let Some(system) = object.get("system").filter(|system| !system.is_null()) {
                found = true;
                push_text(system, &mut text, &mut references);
            }
            object
                .get("messages")
                .and_then(Value::as_array)
                .unwrap_or(&empty)
        }
        // A bare prompt carries no system message.
        _ => return Some(Fingerprint::none()),
    };
    for message in messages {
        let Some(object) = message.as_object() else {
            continue;
        };
        let is_system = object
            .get("role")
            .and_then(Value::as_str)
            .is_some_and(|role| {
                role.eq_ignore_ascii_case("system") || role.eq_ignore_ascii_case("developer")
            });
        if !is_system {
            continue;
        }
        if found {
            text.push_str("\n\n");
        }
        found = true;
        for key in ["content", "parts"] {
            if let Some(value) = object.get(key) {
                push_text(value, &mut text, &mut references);
            }
        }
    }
    if !found {
        return Some(Fingerprint::none());
    }
    let masked = mask_volatile(&text);
    // A payload reference's address is a content hash: identity for an
    // offloaded system prompt, and exactly the kind of long hex run masking
    // would erase — so it is kept apart and digested unmasked.
    let mut identity = masked.clone().into_bytes();
    identity.extend_from_slice(references.as_bytes());
    let digest = digest16(&identity);
    let mut sample = masked;
    if !references.is_empty() {
        sample.push_str(&references);
    }
    Some(Fingerprint {
        key: digest.clone(),
        display: digest,
        sample: Some(Sample::Text(truncate_bytes(&sample, SAMPLE_BYTES))),
    })
}

/// Appends the text a message `content` or `parts` value carries: strings as
/// themselves, text parts by their `text` or `content`, anything else as its
/// canonical JSON. Payload references go to `references` instead.
fn push_text(value: &Value, text: &mut String, references: &mut String) {
    match value {
        Value::String(part) => text.push_str(part),
        Value::Array(parts) => {
            for part in parts {
                push_text(part, text, references);
            }
        }
        Value::Object(object) => {
            if let Some(Value::String(address)) = object.get(crate::payload::PAYLOAD_KEY) {
                references.push_str("\n[payload ");
                references.push_str(address);
                references.push(']');
            } else if let Some(Value::String(part)) = object.get("text") {
                text.push_str(part);
            } else if let Some(Value::String(part)) = object.get("content") {
                text.push_str(part);
            } else {
                text.push_str(&crate::evals::canonical_json(value));
            }
        }
        Value::Null => {}
        other => text.push_str(&other.to_string()),
    }
}

/// The tool catalog a model call was offered, per the module table.
fn tool_catalog(attributes: &Map<String, Value>, request: Option<&Value>) -> Option<Fingerprint> {
    let definitions: Vec<Value> = if let Some(value) = attributes.get(GEN_AI_TOOL_DEFINITIONS) {
        match semconv::field_value(value) {
            Value::Array(items) => items.iter().map(semconv::field_value).collect(),
            Value::Null => return None,
            Value::String(text) if text.is_empty() => return None,
            other => vec![other],
        }
    } else if let Some(definitions) = indexed(attributes, LLM_REQUEST_FUNCTIONS) {
        definitions
    } else if let Some(schemas) = indexed(attributes, OI_TOOLS) {
        let definitions: Vec<Value> = schemas
            .iter()
            .filter_map(|tool| tool.get(OI_TOOL_SCHEMA).map(semconv::field_value))
            .collect();
        if definitions.is_empty() {
            return None;
        }
        definitions
    } else {
        // A provider request body's own `tools` (OpenInference `input.value`).
        request?
            .as_object()?
            .get("tools")?
            .as_array()?
            .iter()
            .map(semconv::field_value)
            .collect()
    };
    if definitions.is_empty() {
        let mut none = Fingerprint::none();
        none.sample = Some(Sample::Names(Vec::new()));
        return Some(none);
    }
    let mut canonical: Vec<String> = definitions
        .iter()
        .map(crate::evals::canonical_json)
        .collect();
    canonical.sort();
    let digest = digest16(canonical.join("\n").as_bytes());
    let mut names: Vec<String> = definitions.iter().filter_map(tool_name).collect();
    names.sort();
    names.dedup();
    names.truncate(MAX_CATALOG_NAMES);
    Some(Fingerprint {
        key: digest.clone(),
        display: digest,
        sample: Some(Sample::Names(names)),
    })
}

/// A tool definition's name: `name`, else `function.name` (the OpenAI shape).
fn tool_name(definition: &Value) -> Option<String> {
    let name = definition.get("name").and_then(Value::as_str).or_else(|| {
        definition
            .get("function")
            .and_then(|function| function.get("name"))
            .and_then(Value::as_str)
    })?;
    Some(clip(name, MAX_VALUE_CHARS))
}

/// Reassembles `{prefix}{i}.{field}` attributes into objects ordered by
/// `i`, each keyed by the rest of the attribute name. `None` when there are
/// none.
fn indexed(attributes: &Map<String, Value>, prefix: &str) -> Option<Vec<Value>> {
    let mut objects: BTreeMap<u64, Map<String, Value>> = BTreeMap::new();
    for (key, value) in attributes {
        let Some((index, field)) = key
            .strip_prefix(prefix)
            .and_then(|rest| rest.split_once('.'))
        else {
            continue;
        };
        let Ok(index) = index.parse::<u64>() else {
            continue;
        };
        if value.is_null() {
            continue;
        }
        objects
            .entry(index)
            .or_default()
            .insert(field.to_owned(), semconv::field_value(value));
    }
    (!objects.is_empty()).then(|| objects.into_values().map(Value::Object).collect())
}

/// `text` cut to at most `max_bytes` at a character boundary.
fn truncate_bytes(text: &str, max_bytes: usize) -> String {
    let mut end = max_bytes.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// Masks the volatile tokens the module documents as `<v>`.
///
/// Byte-level and ASCII-only: every pattern is ASCII, so a multi-byte
/// character is never split, and the output is valid UTF-8 whenever the
/// input is. A token is only masked whole — it must start and end at a
/// boundary (a byte that is not an ASCII letter or digit).
pub(crate) fn mask_volatile(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let at_boundary = index == 0 || !bytes[index - 1].is_ascii_alphanumeric();
        if at_boundary && bytes[index].is_ascii_alphanumeric() {
            if let Some(length) = volatile_at(&bytes[index..]) {
                out.extend_from_slice(b"<v>");
                index += length;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| text.to_owned())
}

/// The length of the volatile token starting `bytes`, if one does.
fn volatile_at(bytes: &[u8]) -> Option<usize> {
    let ends_at = |length: usize| {
        bytes
            .get(length)
            .map_or(true, |byte| !byte.is_ascii_alphanumeric())
    };
    if is_uuid(bytes) && ends_at(36) {
        return Some(36);
    }
    if let Some(length) = date_time(bytes) {
        if ends_at(length) {
            return Some(length);
        }
    }
    if let Some(length) = clock(bytes) {
        if ends_at(length) {
            return Some(length);
        }
    }
    let digits = bytes
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digits >= 6 && ends_at(digits) {
        return Some(digits);
    }
    let hex = bytes
        .iter()
        .take_while(|byte| byte.is_ascii_hexdigit())
        .count();
    if hex >= 16 && ends_at(hex) {
        return Some(hex);
    }
    None
}

fn digits_at(bytes: &[u8], from: usize, count: usize) -> bool {
    bytes.len() >= from + count && bytes[from..from + count].iter().all(u8::is_ascii_digit)
}

fn is_uuid(bytes: &[u8]) -> bool {
    if bytes.len() < 36 {
        return false;
    }
    bytes[..36]
        .iter()
        .enumerate()
        .all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

/// `YYYY-MM-DD`, optionally followed by `T` or a space and a clock time, and
/// then an optional `Z` or `±HH:MM`.
fn date_time(bytes: &[u8]) -> Option<usize> {
    let date = digits_at(bytes, 0, 4)
        && bytes.get(4) == Some(&b'-')
        && digits_at(bytes, 5, 2)
        && bytes.get(7) == Some(&b'-')
        && digits_at(bytes, 8, 2);
    if !date {
        return None;
    }
    let mut length = 10;
    if matches!(bytes.get(10), Some(b'T') | Some(b' ')) {
        if let Some(time) = clock(&bytes[11..]) {
            length = 11 + time;
            match bytes.get(length) {
                Some(b'Z') => length += 1,
                Some(b'+') | Some(b'-')
                    if digits_at(bytes, length + 1, 2)
                        && bytes.get(length + 3) == Some(&b':')
                        && digits_at(bytes, length + 4, 2) =>
                {
                    length += 6;
                }
                _ => {}
            }
        }
    }
    Some(length)
}

/// `H:MM` or `HH:MM`, optionally `:SS` and a fraction.
fn clock(bytes: &[u8]) -> Option<usize> {
    let hours = bytes
        .iter()
        .take(2)
        .take_while(|b| b.is_ascii_digit())
        .count();
    if hours == 0 || bytes.get(hours) != Some(&b':') || !digits_at(bytes, hours + 1, 2) {
        return None;
    }
    let mut length = hours + 3;
    if bytes.get(length) == Some(&b':') && digits_at(bytes, length + 1, 2) {
        length += 3;
        if bytes.get(length) == Some(&b'.') {
            let fraction = bytes[length + 1..]
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            if fraction > 0 {
                length += 1 + fraction;
            }
        }
    }
    Some(length)
}

/// Where two texts first differ: the character offset, a little context
/// before it, and each side's differing stretch (common suffix removed),
/// each cut to `chars` characters.
pub(crate) fn first_divergence(
    before: &str,
    after: &str,
    chars: usize,
) -> Option<(usize, String, String, String)> {
    if before == after {
        return None;
    }
    let a: Vec<char> = before.chars().collect();
    let b: Vec<char> = after.chars().collect();
    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let max_suffix = a.len().min(b.len()) - prefix;
    let suffix = a
        .iter()
        .rev()
        .zip(b.iter().rev())
        .take(max_suffix)
        .take_while(|(x, y)| x == y)
        .count();
    let context_start = prefix.saturating_sub(24);
    let context: String = a[context_start..prefix].iter().collect();
    let side = |text: &[char]| {
        let stretch: String = text[prefix..text.len() - suffix].iter().collect();
        clip(&stretch, chars)
    };
    Some((prefix, context, side(&a), side(&b)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn llm_span(attributes: Value) -> Span {
        Span {
            trace_id: "t".into(),
            span_id: "s".into(),
            tenant: String::new(),
            parent_span_id: None,
            name: "chat".into(),
            start_time_ns: 1,
            end_time_ns: 2,
            status: "ok".into(),
            service: "svc".into(),
            attributes: attributes.as_object().cloned().unwrap_or_default(),
            events: Vec::new(),
            links: Vec::new(),
            extra: Map::new(),
        }
    }

    fn dimension(span: &Span, wanted: Dimension) -> Option<Fingerprint> {
        llm_config(span)
            .into_iter()
            .find(|(dimension, _)| *dimension == wanted)
            .map(|(_, fingerprint)| fingerprint)
    }

    #[test]
    fn volatile_tokens_are_masked_whole_and_versions_survive() {
        assert_eq!(
            mask_volatile(
                "Today is 2026-10-17T09:30:00Z; request 0f8fad5b-d9cb-469f-a165-70867728950e"
            ),
            "Today is <v>; request <v>"
        );
        assert_eq!(mask_volatile("at 9:05 or 14:03:59.123."), "at <v> or <v>.");
        assert_eq!(
            mask_volatile("id 1712345678 sha 9f86d081884c7d65"),
            "id <v> sha <v>"
        );
        // Short numbers, versions and words containing digits are not volatile.
        assert_eq!(
            mask_volatile("policy v2, model gpt-4o, 12345"),
            "policy v2, model gpt-4o, 12345"
        );
        assert_eq!(
            mask_volatile("abc1712345678"),
            "abc1712345678",
            "not a whole token"
        );
        assert_eq!(
            mask_volatile("1712345678abc"),
            "1712345678abc",
            "not a whole token"
        );
        assert_eq!(
            mask_volatile("héllo 2026-01-02 ✓"),
            "héllo <v> ✓",
            "UTF-8 is untouched"
        );
    }

    #[test]
    fn the_system_prompt_ignores_volatile_tokens_and_sees_edits() {
        let prompt = |text: &str| {
            let span = llm_span(json!({
                "gen_ai.request.model": "m",
                "gen_ai.input.messages": json!([
                    {"role": "system", "content": text},
                    {"role": "user", "content": "hi"}
                ]).to_string(),
            }));
            dimension(&span, Dimension::SystemPrompt).expect("has one")
        };
        let monday = prompt("You help. Today is 2026-10-12.");
        let tuesday = prompt("You help. Today is 2026-10-13.");
        let edited = prompt("You help briefly. Today is 2026-10-13.");
        assert_eq!(monday.key, tuesday.key, "the date is volatile");
        assert_ne!(monday.key, edited.key, "the edit is not");
        assert_eq!(monday.key.len(), 16);
        assert_eq!(
            monday.sample,
            Some(Sample::Text("You help. Today is <v>.".to_owned()))
        );
    }

    #[test]
    fn the_system_prompt_reads_every_convention_alike() {
        let otel = llm_span(json!({
            "gen_ai.request.model": "m",
            "gen_ai.system_instructions": json!([{"type": "text", "content": "Be terse."}]).to_string(),
            "gen_ai.input.messages": json!([{"role": "user", "content": "x"}]).to_string(),
        }));
        let openllmetry = llm_span(json!({
            "gen_ai.request.model": "m",
            "gen_ai.prompt.0.role": "system",
            "gen_ai.prompt.0.content": "Be terse.",
            "gen_ai.prompt.1.role": "user",
            "gen_ai.prompt.1.content": "x",
        }));
        let openai_parts = llm_span(json!({
            "gen_ai.request.model": "m",
            "gen_ai.input.messages": json!([
                {"role": "developer", "content": [{"type": "text", "text": "Be terse."}]}
            ]).to_string(),
        }));
        // OpenInference's raw request body, in both providers' shapes.
        let openai_body = llm_span(json!({
            "openinference.span.kind": "LLM",
            "llm.model_name": "m",
            "input.value": json!({"model": "m", "messages": [
                {"role": "system", "content": "Be terse."}, {"role": "user", "content": "x"}
            ]}).to_string(),
        }));
        let anthropic_body = llm_span(json!({
            "openinference.span.kind": "LLM",
            "llm.model_name": "m",
            "input.value": json!({"model": "m", "system": "Be terse.",
                                  "messages": [{"role": "user", "content": "x"}]}).to_string(),
        }));
        let key = |span: &Span| dimension(span, Dimension::SystemPrompt).map(|f| f.key);
        assert_eq!(key(&otel), key(&openllmetry));
        assert_eq!(key(&otel), key(&openai_parts));
        assert_eq!(key(&otel), key(&openai_body));
        assert_eq!(key(&otel), key(&anthropic_body));
        let no_system = llm_span(json!({
            "gen_ai.request.model": "m",
            "gen_ai.prompt.0.role": "user",
            "gen_ai.prompt.0.content": "x",
        }));
        assert_eq!(
            dimension(&no_system, Dimension::SystemPrompt).map(|f| f.display),
            Some(NONE.to_owned()),
            "no system message is a configuration"
        );
        let uncaptured = llm_span(json!({"gen_ai.request.model": "m"}));
        assert_eq!(
            dimension(&uncaptured, Dimension::SystemPrompt),
            None,
            "an uncaptured request is unknown, not (none)"
        );
        let offloaded = llm_span(json!({
            "gen_ai.request.model": "m",
            "gen_ai.input.messages": {"$payload": format!("sha256/{}", "a".repeat(64))},
        }));
        assert_eq!(dimension(&offloaded, Dimension::SystemPrompt), None);
    }

    #[test]
    fn an_offloaded_system_prompt_is_identified_by_its_address() {
        let reference = |hex: char| {
            llm_span(json!({
                "gen_ai.request.model": "m",
                "gen_ai.system_instructions": {"$payload": format!("sha256/{}", hex.to_string().repeat(64))},
                "gen_ai.input.messages": json!([{"role": "user", "content": "x"}]).to_string(),
            }))
        };
        let key = |span: &Span| dimension(span, Dimension::SystemPrompt).map(|f| f.key);
        assert_ne!(key(&reference('a')), key(&reference('b')));
        assert_eq!(key(&reference('a')), key(&reference('a')));
    }

    #[test]
    fn the_tool_catalog_is_order_free_and_names_its_tools() {
        let search = json!({"name": "search", "parameters": {"type": "object"}});
        let read = json!({"type": "function", "function": {"name": "read_file"}});
        let forward = llm_span(json!({
            "gen_ai.tool.definitions": json!([search, read]).to_string(),
        }));
        let backward = llm_span(json!({
            "gen_ai.tool.definitions": [read.to_string(), search.to_string()],
        }));
        let a = dimension(&forward, Dimension::ToolCatalog).expect("catalog");
        let b = dimension(&backward, Dimension::ToolCatalog).expect("catalog");
        assert_eq!(a.key, b.key, "definition order does not matter");
        assert_eq!(
            a.sample,
            Some(Sample::Names(vec![
                "read_file".to_owned(),
                "search".to_owned()
            ]))
        );
        let openllmetry = llm_span(json!({
            "llm.request.functions.0.name": "search",
            "llm.request.functions.0.parameters": "{\"type\":\"object\"}",
        }));
        let c = dimension(&openllmetry, Dimension::ToolCatalog).expect("catalog");
        assert_eq!(c.sample, Some(Sample::Names(vec!["search".to_owned()])));
        let openinference = llm_span(json!({
            "llm.model_name": "m",
            "llm.tools.0.tool.json_schema": search.to_string(),
        }));
        let d = dimension(&openinference, Dimension::ToolCatalog).expect("catalog");
        assert_eq!(d.sample, c.sample);
        let body = llm_span(json!({
            "openinference.span.kind": "LLM",
            "llm.model_name": "m",
            "input.value": json!({"model": "m", "tools": [search], "messages": []}).to_string(),
        }));
        let e = dimension(&body, Dimension::ToolCatalog).expect("a request body's tools");
        assert_eq!(
            e.key, d.key,
            "the same definition, wherever it was recorded"
        );
        let none = llm_span(json!({"gen_ai.tool.definitions": "[]"}));
        assert_eq!(
            dimension(&none, Dimension::ToolCatalog).map(|f| f.display),
            Some(NONE.to_owned())
        );
    }

    #[test]
    fn models_params_and_versions_are_read_from_their_keys() {
        let span = llm_span(json!({
            "gen_ai.request.model": "gpt-4o",
            "gen_ai.response.model": "gpt-4o-2024-08-06",
            "gen_ai.request.temperature": 0.2,
            "gen_ai.request.max_tokens": 1024,
            "llm.invocation_parameters": "{\"model\":\"x\",\"top_p\":0.9,\"temperature\":0.7}",
            "service.version": "1.5.0",
            "telemetry.sdk.name": "opentelemetry",
            "telemetry.sdk.version": "1.27.0",
            "deployment.environment": "prod",
        }));
        assert_eq!(
            dimension(&span, Dimension::ModelRequested).map(|f| f.display),
            Some("gpt-4o".to_owned())
        );
        assert_eq!(
            dimension(&span, Dimension::ModelReturned).map(|f| f.display),
            Some("gpt-4o-2024-08-06".to_owned())
        );
        assert_eq!(
            dimension(&span, Dimension::Params).map(|f| f.display),
            Some("max_tokens=1024, temperature=0.2, top_p=0.9".to_owned()),
            "the gen_ai key wins a collision, and the model is its own dimension"
        );
        assert_eq!(
            service_version(&span.attributes).map(|f| f.display),
            Some("1.5.0".to_owned())
        );
        assert_eq!(
            sdk_version(&span.attributes).map(|f| f.display),
            Some("opentelemetry/1.27.0".to_owned())
        );
        assert_eq!(environment(&span.attributes), Some("prod".to_owned()));
    }

    #[test]
    fn long_values_are_identified_by_digest() {
        let long = "x".repeat(400);
        let fingerprint = Fingerprint::value(&long);
        assert!(fingerprint.key.starts_with('#'));
        assert_eq!(fingerprint.display.chars().count(), MAX_VALUE_CHARS + 1);
        assert_ne!(fingerprint.key, Fingerprint::value(&"y".repeat(400)).key);
    }

    #[test]
    fn the_first_divergence_names_the_changed_stretch() {
        let (offset, context, before, after) = first_divergence(
            "You are helpful. Always answer in English. Be brief.",
            "You are helpful. Reply in the user's language. Be brief.",
            80,
        )
        .expect("differs");
        assert_eq!(offset, 17);
        assert_eq!(context, "You are helpful. ");
        assert_eq!(before, "Always answer in English");
        assert_eq!(after, "Reply in the user's language");
        assert_eq!(first_divergence("same", "same", 80), None);
        let (_, _, before, after) = first_divergence("ab", "abc", 80).expect("differs");
        assert_eq!((before.as_str(), after.as_str()), ("", "c"));
    }
}
