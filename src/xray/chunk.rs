//! A model call's context window as chunks.
//!
//! Every recognized message shape folds to the same unit: one chunk per
//! content part. A plain-text message is one chunk; an assistant message with
//! text and two tool calls is three; a user message carrying two tool results
//! (the Anthropic shape) is two. Tool definitions the span recorded come first,
//! as their own chunks, because a provider's cached prefix starts with them.
//!
//! Each chunk carries two digests, kept apart on purpose:
//!
//! - **content** — of the chunk's canonical text: a JSON object or array held
//!   as text is canonicalized (keys sorted, no whitespace), anything else is
//!   the text itself; role and position are not part of it. Provenance,
//!   duplicates and compaction loss compare this, so the same tool output
//!   matches whether the tool span stored it parsed and the next prompt
//!   carried it as text, or the other way round.
//! - **exact** — of the role, the kind, the tool-call id and the text exactly
//!   as recorded. The cache autopsy compares this, because a provider's
//!   prefix cache matches bytes: the same content re-serialized with its keys
//!   in another order is a cache miss even though it is not new content.
//!
//! Digests are Traza's 128-bit [`hash128`](crate::hash::hash128) — fast enough
//! to digest every prompt of a long run — and are reported as their first 16
//! hex characters. They are identities for comparison inside one response,
//! not commitments: a producer able to choose its own telemetry text can
//! manufacture a collision, which costs it a wrong answer about its own data
//! in its own tenant and nothing else.

use serde::Serialize;
use serde_json::{Map, Value};

/// What a chunk is, as far as the reader of a context window cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkKind {
    /// System or developer instructions.
    System,
    /// What a user (or the harness, in the user's turn) said.
    User,
    /// The model's own text, reasoning included.
    Assistant,
    /// A tool's output, as fed back to the model.
    ToolResult,
    /// A tool call the model made.
    ToolCall,
    /// Anything else: tool definitions, media, an unrecognized part.
    Other,
}

impl ChunkKind {
    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::ToolResult => "tool_result",
            Self::ToolCall => "tool_call",
            Self::Other => "other",
        }
    }

    fn tag(self) -> u8 {
        match self {
            Self::System => 1,
            Self::User => 2,
            Self::Assistant => 3,
            Self::ToolResult => 4,
            Self::ToolCall => 5,
            Self::Other => 6,
        }
    }
}

/// The pseudo-role tool definitions are reported under.
pub(crate) const TOOLS_ROLE: &str = "tools";

/// One chunk before analysis.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Piece {
    /// The message role as recorded (lowercased), or [`TOOLS_ROLE`].
    pub(crate) role: Option<String>,
    /// The chunk's kind.
    pub(crate) kind: ChunkKind,
    /// The chunk's text as recorded; for a tool call, `name(arguments)`.
    pub(crate) text: String,
    /// Digest of the canonical content.
    pub(crate) content: u128,
    /// Digest of role, kind, call id and exact text.
    pub(crate) exact: u128,
    /// The tool call id a tool call or tool result carries.
    pub(crate) tool_call_id: Option<String>,
    /// The tool a call or a definition names.
    pub(crate) name: Option<String>,
    /// A media part (image, audio, file): its tokens are not estimated.
    pub(crate) media: bool,
}

impl Piece {
    /// Estimated tokens: about four ASCII characters, or one and a half
    /// other characters, per token. An estimate, not a tokenizer; media
    /// parts estimate to zero rather than to the length of their encoding.
    pub(crate) fn est_tokens(&self) -> u64 {
        if self.media {
            return 0;
        }
        est_tokens(&self.text)
    }

    /// Characters (Unicode scalar values) of the text.
    pub(crate) fn chars(&self) -> usize {
        self.text.chars().count()
    }
}

/// See [`Piece::est_tokens`].
pub(crate) fn est_tokens(text: &str) -> u64 {
    let (mut ascii, mut other) = (0u64, 0u64);
    for character in text.chars() {
        if character.is_ascii() {
            ascii += 1;
        } else {
            other += 1;
        }
    }
    // ascii/4 + other/1.5, rounded up, in integers: (3·ascii + 8·other) / 12.
    (3 * ascii + 8 * other).div_ceil(12)
}

/// The first 16 hex characters of a digest, for the wire.
pub(crate) fn digest_hex(digest: u128) -> String {
    let bytes = digest.to_le_bytes();
    let mut out = String::with_capacity(16);
    for byte in &bytes[..8] {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// `text` cut to `max` characters, with an ellipsis when it was cut.
pub(crate) fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        None => text.to_owned(),
        Some((cut, _)) => format!("{}…", &text[..cut]),
    }
}

fn hash(bytes: &[u8]) -> u128 {
    u128::from_le_bytes(crate::hash::hash128(bytes).0)
}

/// Collects pieces up to a cap, saying whether it had to stop.
pub(crate) struct Pieces {
    pub(crate) list: Vec<Piece>,
    cap: usize,
    /// Whether more pieces were offered than the cap allowed.
    pub(crate) truncated: bool,
}

impl Pieces {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            list: Vec::new(),
            cap,
            truncated: false,
        }
    }

    fn full(&mut self) -> bool {
        if self.list.len() >= self.cap {
            self.truncated = true;
            return true;
        }
        false
    }

    /// A text chunk.
    fn text(
        &mut self,
        role: Option<&str>,
        kind: ChunkKind,
        text: String,
        tool_call_id: Option<String>,
    ) {
        if self.full() {
            return;
        }
        let text_hash = hash(text.as_bytes());
        let content = canonical_digest(&text).unwrap_or(text_hash);
        let exact = exact_digest(role, kind, tool_call_id.as_deref(), text_hash);
        self.list.push(Piece {
            role: role.map(str::to_owned),
            kind,
            text,
            content,
            exact,
            tool_call_id,
            name: None,
            media: false,
        });
    }

    /// A tool call: `name(arguments)`, its content digest covering the id,
    /// the name and the canonical arguments.
    fn tool_call(&mut self, role: Option<&str>, call: &Value) {
        if self.full() {
            return;
        }
        let object = call.as_object();
        let function = object
            .and_then(|object| object.get("function"))
            .and_then(Value::as_object);
        let field = |keys: &[&str]| {
            keys.iter().find_map(|key| {
                object
                    .and_then(|object| object.get(*key))
                    .or_else(|| function.and_then(|function| function.get(*key)))
                    .filter(|value| !value.is_null())
            })
        };
        let name = field(&["name", "tool_name"]).and_then(scalar_text);
        let arguments = field(&["arguments", "input", "args", "parameters"])
            .map(crate::semconv::field_value)
            .unwrap_or(Value::Null);
        let id = field(&["id", "call_id", "tool_call_id"]).and_then(scalar_text);
        let arguments_text = match &arguments {
            Value::Null => String::new(),
            Value::String(text) => text.clone(),
            other => crate::evals::canonical_json(other),
        };
        let text = format!("{}({arguments_text})", name.as_deref().unwrap_or("?"));
        let mut identity = Map::new();
        identity.insert(
            "id".to_owned(),
            id.clone().map_or(Value::Null, Value::String),
        );
        identity.insert(
            "name".to_owned(),
            name.clone().map_or(Value::Null, Value::String),
        );
        identity.insert("arguments".to_owned(), arguments);
        let content = hash(crate::evals::canonical_json(&Value::Object(identity)).as_bytes());
        let role = role.or(Some("assistant"));
        let exact = exact_digest(
            role,
            ChunkKind::ToolCall,
            id.as_deref(),
            hash(text.as_bytes()),
        );
        self.list.push(Piece {
            role: role.map(str::to_owned),
            kind: ChunkKind::ToolCall,
            text,
            content,
            exact,
            tool_call_id: id,
            name,
            media: false,
        });
    }

    /// A media part: digested by its whole value, shown as a placeholder.
    fn media(&mut self, role: Option<&str>, part: &Map<String, Value>) {
        if self.full() {
            return;
        }
        let canonical = crate::evals::canonical_json(&Value::Object(part.clone()));
        let kind = part.get("type").and_then(Value::as_str).unwrap_or("media");
        let mime = ["mime_type", "media_type", "mimeType"]
            .iter()
            .find_map(|key| part.get(*key).and_then(Value::as_str))
            .or_else(|| {
                part.get("source")
                    .and_then(|source| source.get("media_type"))
                    .and_then(Value::as_str)
            });
        let text = match mime {
            Some(mime) => format!("[{kind}: {mime}, {} bytes recorded]", canonical.len()),
            None => format!("[{kind}: {} bytes recorded]", canonical.len()),
        };
        let content = hash(canonical.as_bytes());
        let exact = exact_digest(role, ChunkKind::Other, None, content);
        self.list.push(Piece {
            role: role.map(str::to_owned),
            kind: ChunkKind::Other,
            text,
            content,
            exact,
            tool_call_id: None,
            name: None,
            media: true,
        });
    }

    /// A tool definition, reported under [`TOOLS_ROLE`].
    fn definition(&mut self, definition: &Value) {
        if self.full() {
            return;
        }
        let definition = crate::semconv::field_value(definition);
        let name = ["name", "tool_name"]
            .iter()
            .find_map(|key| definition.get(*key))
            .or_else(|| definition.get("function").and_then(|f| f.get("name")))
            .and_then(scalar_text);
        let text = match &definition {
            Value::String(text) => text.clone(),
            other => crate::evals::canonical_json(other),
        };
        let text_hash = hash(text.as_bytes());
        let content = canonical_digest(&text).unwrap_or(text_hash);
        let exact = exact_digest(Some(TOOLS_ROLE), ChunkKind::Other, None, text_hash);
        self.list.push(Piece {
            role: Some(TOOLS_ROLE.to_owned()),
            kind: ChunkKind::Other,
            text,
            content,
            exact,
            tool_call_id: None,
            name,
            media: false,
        });
    }
}

/// The digest of a JSON-shaped text's canonical form, or `None` when the text
/// is not a JSON object or array (its content digest is then its own).
fn canonical_digest(text: &str) -> Option<u128> {
    let trimmed = text.trim_start();
    if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
        return None;
    }
    let parsed: Value = serde_json::from_str(text).ok()?;
    Some(hash(crate::evals::canonical_json(&parsed).as_bytes()))
}

fn exact_digest(role: Option<&str>, kind: ChunkKind, id: Option<&str>, text_hash: u128) -> u128 {
    let mut frame = Vec::with_capacity(64);
    let role = role.unwrap_or("");
    frame.extend_from_slice(&(role.len() as u32).to_le_bytes());
    frame.extend_from_slice(role.as_bytes());
    frame.push(kind.tag());
    match id {
        Some(id) => {
            frame.push(1);
            frame.extend_from_slice(&(id.len() as u32).to_le_bytes());
            frame.extend_from_slice(id.as_bytes());
        }
        None => frame.push(0),
    }
    frame.extend_from_slice(&text_hash.to_le_bytes());
    hash(&frame)
}

/// A scalar's text: strings as themselves, numbers spelled out.
fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// The kind a message role implies for its plain-text content.
fn role_kind(role: Option<&str>) -> ChunkKind {
    match role {
        Some("system" | "developer") => ChunkKind::System,
        Some("user" | "human") => ChunkKind::User,
        Some("assistant" | "model" | "ai" | "bot") => ChunkKind::Assistant,
        Some("tool" | "function" | "ipython" | "tool_result") => ChunkKind::ToolResult,
        _ => ChunkKind::Other,
    }
}

/// Text of a tool result's content, whichever shape carried it: a string; a
/// list of text parts (joined by newlines); a content envelope (`{"type":
/// "text", "text": …}`, `{"content": …}`); otherwise canonical JSON.
pub(crate) fn content_text(value: &Value) -> String {
    const ENVELOPE: &[&str] = &[
        "type",
        "text",
        "content",
        "is_error",
        "isError",
        "annotations",
    ];
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| match item {
                Value::String(text) => text.clone(),
                Value::Object(object)
                    if object.keys().all(|key| ENVELOPE.contains(&key.as_str())) =>
                {
                    content_text(item)
                }
                other => crate::evals::canonical_json(other),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(object) if object.keys().all(|key| ENVELOPE.contains(&key.as_str())) => {
            match (object.get("text"), object.get("content")) {
                (Some(Value::String(text)), _) => text.clone(),
                (_, Some(content)) => content_text(content),
                _ => crate::evals::canonical_json(value),
            }
        }
        other => crate::evals::canonical_json(other),
    }
}

/// Lowercased role of a message object, if it names one.
fn role_of(message: &Map<String, Value>) -> Option<String> {
    message
        .get("role")
        .and_then(Value::as_str)
        .filter(|role| !role.is_empty())
        .map(str::to_ascii_lowercase)
}

/// The chunks of a model call's context window: the tool definitions the
/// span recorded, then the request's messages in order.
///
/// `request` is the step's request as [`crate::semconv::step_facts`]
/// extracts it (system instructions already folded in as the first message),
/// resolved or not; `definitions` the tool-definition values found on the
/// span ([`tool_definitions`]).
pub(crate) fn context_pieces(request: Option<&Value>, definitions: &[Value], cap: usize) -> Pieces {
    let mut pieces = Pieces::new(cap);
    for definition in definitions {
        match crate::semconv::field_value(definition) {
            Value::Array(list) => list.iter().for_each(|item| pieces.definition(item)),
            other => pieces.definition(&other),
        }
    }
    if let Some(request) = request {
        if let Some(body_tools) = request
            .as_object()
            .filter(|body| body.contains_key("messages"))
            .and_then(|body| body.get("tools"))
            .and_then(Value::as_array)
        {
            if definitions.is_empty() {
                body_tools.iter().for_each(|tool| pieces.definition(tool));
            }
        }
        messages(&mut pieces, request, None);
    }
    pieces
}

/// The chunks of a model call's output.
pub(crate) fn response_pieces(response: &Value, cap: usize) -> Pieces {
    let mut pieces = Pieces::new(cap);
    messages(&mut pieces, response, Some("assistant"));
    pieces
}

/// One value as one chunk: a tool's output, or an agent's input or output.
pub(crate) fn value_piece(
    value: &Value,
    kind: ChunkKind,
    tool_call_id: Option<String>,
) -> Option<Piece> {
    let mut pieces = Pieces::new(1);
    let text = content_text(value);
    if text.is_empty() {
        return None;
    }
    pieces.text(None, kind, text, tool_call_id);
    pieces.list.pop()
}

/// Tool definitions on a span: `gen_ai.tool.definitions`, OpenLLMetry's
/// indexed `llm.request.functions.{i}.*`, or OpenInference's
/// `llm.tools.{i}.tool.json_schema` — the first convention present.
pub(crate) fn tool_definitions(attributes: &Map<String, Value>) -> Vec<Value> {
    if let Some(value) = attributes
        .get("gen_ai.tool.definitions")
        .filter(|value| !value.is_null())
    {
        return vec![value.clone()];
    }
    let mut functions: std::collections::BTreeMap<u64, Map<String, Value>> =
        std::collections::BTreeMap::new();
    for (key, value) in attributes {
        if let Some((index, field)) = key
            .strip_prefix("llm.request.functions.")
            .and_then(|rest| rest.split_once('.'))
        {
            if let Ok(index) = index.parse::<u64>() {
                functions
                    .entry(index)
                    .or_default()
                    .insert(field.to_owned(), crate::semconv::field_value(value));
            }
        }
    }
    if !functions.is_empty() {
        return functions.into_values().map(Value::Object).collect();
    }
    let mut schemas: std::collections::BTreeMap<u64, Value> = std::collections::BTreeMap::new();
    for (key, value) in attributes {
        if let Some(index) = key
            .strip_prefix("llm.tools.")
            .and_then(|rest| rest.strip_suffix(".tool.json_schema"))
        {
            if let Ok(index) = index.parse::<u64>() {
                schemas.insert(index, value.clone());
            }
        }
    }
    schemas.into_values().collect()
}

/// Folds a request or response value into pieces.
fn messages(pieces: &mut Pieces, value: &Value, default_role: Option<&str>) {
    match value {
        Value::Array(items) => {
            for item in items {
                message(pieces, item, default_role);
            }
        }
        Value::Object(body) => {
            if let Some(list) = body.get("messages").and_then(Value::as_array) {
                // A provider request body (OpenInference's `input.value`).
                if let Some(system) = body.get("system").filter(|value| !value.is_null()) {
                    content(pieces, Some("system"), ChunkKind::System, system, None);
                }
                for item in list {
                    message(pieces, item, default_role);
                }
            } else if let Some(choices) = body.get("choices").and_then(Value::as_array) {
                // An OpenAI response body.
                for choice in choices {
                    match choice.get("message").or_else(|| choice.get("delta")) {
                        Some(inner) => message(pieces, inner, Some("assistant")),
                        None => {
                            if let Some(text) = choice.get("text") {
                                content(
                                    pieces,
                                    Some("assistant"),
                                    ChunkKind::Assistant,
                                    text,
                                    None,
                                );
                            }
                        }
                    }
                }
            } else {
                message(pieces, value, default_role);
            }
        }
        other => {
            let kind = match default_role {
                Some(role) => role_kind(Some(role)),
                None => ChunkKind::Other,
            };
            content(pieces, default_role, kind, other, None);
        }
    }
}

/// One message.
fn message(pieces: &mut Pieces, value: &Value, default_role: Option<&str>) {
    let Value::Object(object) = value else {
        let kind = role_kind(default_role);
        content(pieces, default_role, kind, value, None);
        return;
    };
    // A message-shaped object is one with a role or a content carrier; any
    // other object is an opaque value.
    let carriers = ["content", "parts", "text", "tool_calls", "function_call"];
    if object.get("role").is_none() && !carriers.iter().any(|key| object.contains_key(*key)) {
        let kind = role_kind(default_role);
        content(pieces, default_role, kind, value, None);
        return;
    }
    let role = role_of(object).or_else(|| default_role.map(str::to_owned));
    let role = role.as_deref();
    let base = role_kind(role);
    let id = ["tool_call_id", "tool_use_id"]
        .iter()
        .find_map(|key| object.get(*key))
        .and_then(scalar_text);
    for key in ["content", "parts", "text"] {
        if let Some(carried) = object.get(key).filter(|value| !value.is_null()) {
            content(pieces, role, base, carried, id.clone());
            break;
        }
    }
    if let Some(calls) = object.get("tool_calls").and_then(Value::as_array) {
        for call in calls {
            pieces.tool_call(role, call);
        }
    }
    if let Some(call) = object
        .get("function_call")
        .filter(|value| value.is_object())
    {
        pieces.tool_call(role, call);
    }
}

/// A message's content: text, a list of parts, or one part.
fn content(
    pieces: &mut Pieces,
    role: Option<&str>,
    base: ChunkKind,
    value: &Value,
    id: Option<String>,
) {
    match value {
        Value::Null => {}
        Value::String(text) => {
            if !text.is_empty() {
                pieces.text(role, base, text.clone(), id);
            }
        }
        Value::Array(parts) => {
            for item in parts {
                part(pieces, role, base, item, id.clone());
            }
        }
        Value::Object(_) => part(pieces, role, base, value, id),
        other => pieces.text(role, base, other.to_string(), id),
    }
}

/// One content part.
fn part(
    pieces: &mut Pieces,
    role: Option<&str>,
    base: ChunkKind,
    value: &Value,
    id: Option<String>,
) {
    let Value::Object(object) = value else {
        match value {
            Value::String(text) if !text.is_empty() => pieces.text(role, base, text.clone(), id),
            Value::String(_) | Value::Null => {}
            other => pieces.text(
                role,
                ChunkKind::Other,
                crate::evals::canonical_json(other),
                id,
            ),
        }
        return;
    };
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase);
    let first_text = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| object.get(*key).filter(|value| !value.is_null()))
            .map(content_text)
    };
    match kind.as_deref() {
        Some("text" | "input_text" | "output_text") => {
            if let Some(text) = first_text(&["text", "content"]).filter(|text| !text.is_empty()) {
                pieces.text(role, base, text, id);
            }
        }
        Some("thinking" | "reasoning" | "redacted_thinking") => {
            if let Some(text) =
                first_text(&["thinking", "content", "text", "data"]).filter(|text| !text.is_empty())
            {
                pieces.text(role, ChunkKind::Assistant, text, None);
            }
        }
        Some("tool_call" | "tool_use" | "function_call" | "server_tool_use") => {
            pieces.tool_call(role, value);
        }
        Some(
            "tool_call_response"
            | "tool_result"
            | "function_call_output"
            | "function_result"
            | "tool_response",
        ) => {
            let call_id = ["id", "tool_use_id", "call_id", "tool_call_id"]
                .iter()
                .find_map(|key| object.get(*key))
                .and_then(scalar_text)
                .or(id);
            let text = first_text(&["response", "content", "output", "result"]).unwrap_or_default();
            pieces.text(role, ChunkKind::ToolResult, text, call_id);
        }
        Some(
            "image" | "image_url" | "input_image" | "blob" | "uri" | "file" | "document"
            | "input_audio" | "audio" | "input_file",
        ) => pieces.media(role, object),
        _ => {
            if let Some(Value::String(text)) = object.get("text") {
                if !text.is_empty() {
                    pieces.text(role, base, text.clone(), id);
                }
            } else if let Some(call) = object.get("functionCall").filter(|v| v.is_object()) {
                // Gemini's parts.
                pieces.tool_call(role, call);
            } else if let Some(answer) = object.get("functionResponse").filter(|v| v.is_object()) {
                let text = answer
                    .get("response")
                    .map(content_text)
                    .unwrap_or_else(|| content_text(answer));
                let call_id = answer.get("id").and_then(scalar_text).or(id);
                pieces.text(role, ChunkKind::ToolResult, text, call_id);
            } else if let Some(inner) = object.get("content").filter(|value| !value.is_null()) {
                content(pieces, role, base, inner, id);
            } else {
                pieces.text(
                    role,
                    ChunkKind::Other,
                    crate::evals::canonical_json(value),
                    id,
                );
            }
        }
    }
}

/// Distinctive words of a text, hashed: runs of letters, digits and
/// `_ . / -` four to sixty-four characters long. Used to tell whether a
/// model's later output mentions something a tool result introduced.
pub(crate) fn words(text: &str, mut each: impl FnMut(u64)) {
    let is_word = |character: char| character.is_alphanumeric() || "_./-".contains(character);
    let mut start: Option<usize> = None;
    let mut emit = |word: &str| {
        let word = word.trim_matches(|c: char| ".-/".contains(c));
        let length = word.chars().count();
        if (4..=64).contains(&length) {
            let digest = crate::hash::hash128(word.as_bytes()).0;
            let mut low = [0u8; 8];
            low.copy_from_slice(&digest[..8]);
            each(u64::from_le_bytes(low));
        }
    };
    for (at, character) in text.char_indices() {
        match (is_word(character), start) {
            (true, None) => start = Some(at),
            (false, Some(from)) => {
                emit(&text[from..at]);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(from) = start {
        emit(&text[from..]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn kinds(pieces: &Pieces) -> Vec<(Option<&str>, ChunkKind)> {
        pieces
            .list
            .iter()
            .map(|piece| (piece.role.as_deref(), piece.kind))
            .collect()
    }

    #[test]
    fn openai_messages_fold_to_one_chunk_per_part() {
        let request = json!([
            {"role": "system", "content": "You are terse."},
            {"role": "user", "content": "weather?"},
            {"role": "assistant", "content": "checking", "tool_calls": [
                {"id": "c1", "type": "function",
                 "function": {"name": "get_weather", "arguments": "{\"city\": \"Paris\"}"}}
            ]},
            {"role": "tool", "tool_call_id": "c1", "content": "{\"temp\": 21}"},
        ]);
        let pieces = context_pieces(Some(&request), &[], 100);
        assert_eq!(
            kinds(&pieces),
            [
                (Some("system"), ChunkKind::System),
                (Some("user"), ChunkKind::User),
                (Some("assistant"), ChunkKind::Assistant),
                (Some("assistant"), ChunkKind::ToolCall),
                (Some("tool"), ChunkKind::ToolResult),
            ]
        );
        let call = &pieces.list[3];
        assert_eq!(call.text, r#"get_weather({"city":"Paris"})"#);
        assert_eq!(call.tool_call_id.as_deref(), Some("c1"));
        assert_eq!(call.name.as_deref(), Some("get_weather"));
        assert_eq!(pieces.list[4].tool_call_id.as_deref(), Some("c1"));
        assert!(!pieces.truncated);
    }

    #[test]
    fn anthropic_and_otel_parts_are_recognized() {
        let request = json!([
            {"role": "user", "content": [
                {"type": "text", "text": "read it"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
            ]},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "hmm"},
                {"type": "tool_use", "id": "tu1", "name": "read_file", "input": {"path": "a.rs"}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "tu1",
                 "content": [{"type": "text", "text": "fn main() {}"}]},
            ]},
            {"role": "assistant", "parts": [
                {"type": "tool_call", "id": "x1", "name": "grep", "arguments": {"q": "main"}},
            ]},
            {"role": "tool", "parts": [
                {"type": "tool_call_response", "id": "x1", "response": "a.rs:1"},
            ]},
        ]);
        let pieces = context_pieces(Some(&request), &[], 100);
        assert_eq!(
            kinds(&pieces),
            [
                (Some("user"), ChunkKind::User),
                (Some("user"), ChunkKind::Other),
                (Some("assistant"), ChunkKind::Assistant),
                (Some("assistant"), ChunkKind::ToolCall),
                (Some("user"), ChunkKind::ToolResult),
                (Some("assistant"), ChunkKind::ToolCall),
                (Some("tool"), ChunkKind::ToolResult),
            ]
        );
        assert!(pieces.list[1].media);
        assert_eq!(pieces.list[1].est_tokens(), 0);
        assert!(
            pieces.list[1].text.contains("image/png"),
            "{}",
            pieces.list[1].text
        );
        assert_eq!(pieces.list[4].text, "fn main() {}");
        assert_eq!(pieces.list[4].tool_call_id.as_deref(), Some("tu1"));
        assert_eq!(pieces.list[6].tool_call_id.as_deref(), Some("x1"));
    }

    #[test]
    fn content_digests_cross_representations_and_exact_digests_do_not() {
        // A tool span stores its result parsed; the next prompt carries it as
        // text with different spacing. Same content, different bytes.
        let parsed = value_piece(
            &json!({"temp": 21, "sky": "rain"}),
            ChunkKind::ToolResult,
            None,
        )
        .expect("piece");
        let request = json!([{"role": "tool", "content": "{\"sky\": \"rain\", \"temp\": 21}"}]);
        let carried = context_pieces(Some(&request), &[], 10).list.remove(0);
        assert_eq!(parsed.content, carried.content);
        assert_ne!(parsed.exact, carried.exact);

        // The same text under another role is the same content, another
        // exact identity.
        let user = context_pieces(Some(&json!([{"role": "user", "content": "x y"}])), &[], 10);
        let system = context_pieces(
            Some(&json!([{"role": "system", "content": "x y"}])),
            &[],
            10,
        );
        assert_eq!(user.list[0].content, system.list[0].content);
        assert_ne!(user.list[0].exact, system.list[0].exact);
    }

    #[test]
    fn tool_definitions_come_first_from_each_convention() {
        let otel = tool_definitions(
            json!({"gen_ai.tool.definitions": "[{\"name\":\"a\"},{\"name\":\"b\"}]"})
                .as_object()
                .unwrap(),
        );
        let pieces = context_pieces(Some(&json!([{"role": "user", "content": "hi"}])), &otel, 10);
        assert_eq!(pieces.list.len(), 3);
        assert_eq!(pieces.list[0].role.as_deref(), Some(TOOLS_ROLE));
        assert_eq!(pieces.list[1].name.as_deref(), Some("b"));
        let indexed = tool_definitions(
            json!({"llm.request.functions.1.name": "second", "llm.request.functions.0.name": "first",
                   "llm.request.functions.0.parameters": "{\"type\":\"object\"}"})
            .as_object()
            .unwrap(),
        );
        assert_eq!(indexed.len(), 2);
        assert_eq!(indexed[0]["name"], json!("first"));
        assert_eq!(indexed[0]["parameters"]["type"], json!("object"));
        let openinference = tool_definitions(
            json!({"llm.tools.0.tool.json_schema": "{\"name\":\"x\"}"})
                .as_object()
                .unwrap(),
        );
        assert_eq!(openinference.len(), 1);
    }

    #[test]
    fn gemini_and_responses_api_items_are_read() {
        let request = json!([
            {"role": "user", "parts": [{"text": "weather?"}]},
            {"role": "model", "parts": [{"functionCall": {"name": "get_weather", "args": {"city": "Rome"}}}]},
            {"role": "function", "parts": [{"functionResponse": {"name": "get_weather",
                                                                 "response": {"temp": 24}}}]},
            {"type": "function_call", "call_id": "fc_1", "name": "lookup", "arguments": "{\"q\":1}"},
            {"type": "function_call_output", "call_id": "fc_1", "output": "found"},
        ]);
        let pieces = context_pieces(Some(&request), &[], 10);
        assert_eq!(
            kinds(&pieces),
            [
                (Some("user"), ChunkKind::User),
                (Some("model"), ChunkKind::ToolCall),
                (Some("function"), ChunkKind::ToolResult),
                (Some("assistant"), ChunkKind::ToolCall),
                (None, ChunkKind::ToolResult),
            ]
        );
        assert_eq!(pieces.list[1].text, r#"get_weather({"city":"Rome"})"#);
        assert_eq!(pieces.list[2].text, r#"{"temp":24}"#);
        assert_eq!(pieces.list[3].tool_call_id.as_deref(), Some("fc_1"));
        assert_eq!(pieces.list[4].tool_call_id.as_deref(), Some("fc_1"));
        assert_eq!(pieces.list[4].text, "found");
    }

    #[test]
    fn provider_bodies_and_bare_values_are_read() {
        let body = json!({"model": "m", "system": "be brief",
                          "messages": [{"role": "user", "content": "hi"}],
                          "tools": [{"name": "t"}]});
        let pieces = context_pieces(Some(&body), &[], 10);
        assert_eq!(
            kinds(&pieces),
            [
                (Some(TOOLS_ROLE), ChunkKind::Other),
                (Some("system"), ChunkKind::System),
                (Some("user"), ChunkKind::User),
            ]
        );
        let response = json!({"choices": [{"message": {"role": "assistant", "content": "hello"}}]});
        assert_eq!(
            kinds(&response_pieces(&response, 10)),
            [(Some("assistant"), ChunkKind::Assistant)]
        );
        // A bare prompt has no role; a bare answer is the assistant's.
        assert_eq!(
            kinds(&context_pieces(Some(&json!("just text")), &[], 10)),
            [(None, ChunkKind::Other)]
        );
        assert_eq!(
            kinds(&response_pieces(&json!("answer"), 10)),
            [(Some("assistant"), ChunkKind::Assistant)]
        );
    }

    #[test]
    fn the_cap_stops_collection_and_says_so() {
        let request: Vec<Value> = (0..10)
            .map(|i| json!({"role": "user", "content": format!("m{i}")}))
            .collect();
        let pieces = context_pieces(Some(&Value::Array(request)), &[], 4);
        assert_eq!(pieces.list.len(), 4);
        assert!(pieces.truncated);
    }

    #[test]
    fn estimates_and_words_are_deterministic() {
        assert_eq!(est_tokens(""), 0);
        assert_eq!(est_tokens("abcd"), 1);
        assert_eq!(est_tokens("abcde"), 2);
        assert_eq!(est_tokens("日本語"), 2);
        let mut seen = Vec::new();
        words("see src/main.rs, id=abc123 and x", |word| seen.push(word));
        // "see" and "x" are too short; "src/main.rs", "abc123" qualify.
        assert_eq!(seen.len(), 2);
        assert_eq!(digest_hex(0).len(), 16);
        assert_eq!(clip("abcdef", 3), "abc…");
        assert_eq!(clip("abc", 3), "abc");
    }
}
