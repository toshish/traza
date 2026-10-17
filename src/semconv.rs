//! Semantic-convention normalization for LLM/agent spans.
//!
//! Traza recognizes three overlapping vocabularies and folds them to one set
//! of facts for its derived views (sessions, cost/token analytics, the
//! dashboard):
//!
//! - the **OpenLLMetry / OpenTelemetry GenAI** conventions emitted by
//!   Traceloop's `opentelemetry-semantic-conventions-ai` and OTel GenAI
//!   instrumentation — `gen_ai.*`, `llm.usage.*`, and `traceloop.*`;
//! - Traza's own native `llm.*` / `session.id` shorthand (docs/llm-semantics.md);
//! - the **OpenInference** conventions (Arize Phoenix instrumentation) —
//!   `openinference.span.kind`, `llm.model_name`, `llm.token_count.*`,
//!   `input.value` / `output.value` and friends.
//!
//! Extraction is a pure function over a span. The conventions are recognized
//! together; a span carrying only the native `llm.*` keys extracts exactly as
//! it did before this module existed, and OpenInference keys rank after every
//! key that was recognized before them, so each addition is additive: a span
//! that resolved a value before resolves the same value now. When several
//! recognized keys are present the precedence tables in docs/llm-semantics.md
//! (repeated on [`facts`] and [`step_facts`]) decide — most-specific /
//! standard key first for model and per-direction token counts, native
//! `session.id` first for session identity.
//!
//! Three derivations live here: [`facts`] (what a model call cost and used),
//! [`step_facts`] (what kind of step a span is, its input and output, and the
//! fingerprint that identifies a repeat), and [`run_id`] (which run a span
//! belongs to).
//!
//! This module is the single source of truth for the key precedence. The
//! React dashboard mirrors the OpenLLMetry / native part of the [`facts`] and
//! session precedence in `ui/src/lib/spans.js`; it does not yet read the
//! OpenInference keys.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::Span;

// -- OpenTelemetry GenAI / OpenLLMetry (Traceloop) -------------------------
// Current OTel GenAI names first; the deprecated names OTel replaced are kept
// as aliases (`gen_ai.system` → `gen_ai.provider.name`,
// `gen_ai.usage.{prompt,completion}_tokens` → `.{input,output}_tokens`).
const GEN_AI_PROVIDER_NAME: &str = "gen_ai.provider.name";
const GEN_AI_SYSTEM: &str = "gen_ai.system"; // deprecated alias for provider
const GEN_AI_OPERATION_NAME: &str = "gen_ai.operation.name";
const GEN_AI_REQUEST_MODEL: &str = "gen_ai.request.model";
const GEN_AI_RESPONSE_MODEL: &str = "gen_ai.response.model";
const GEN_AI_USAGE_INPUT_TOKENS: &str = "gen_ai.usage.input_tokens";
const GEN_AI_USAGE_PROMPT_TOKENS: &str = "gen_ai.usage.prompt_tokens"; // deprecated
const GEN_AI_USAGE_OUTPUT_TOKENS: &str = "gen_ai.usage.output_tokens";
const GEN_AI_USAGE_COMPLETION_TOKENS: &str = "gen_ai.usage.completion_tokens"; // deprecated
const GEN_AI_USAGE_TOTAL_TOKENS: &str = "gen_ai.usage.total_tokens";
const GEN_AI_CONVERSATION_ID: &str = "gen_ai.conversation.id";
const LLM_USAGE_TOTAL_TOKENS: &str = "llm.usage.total_tokens";
const LLM_REQUEST_TYPE: &str = "llm.request.type";
const TRACELOOP_SPAN_KIND: &str = "traceloop.span.kind";
const TRACELOOP_SESSION_ID: &str = "traceloop.association.properties.session_id";
const TRACELOOP_CHAT_ID: &str = "traceloop.association.properties.chat_id";

// Cost is NOT an OpenTelemetry GenAI attribute; `llm.cost_usd` is a Traza
// extension, populated when a pipeline meters cost. `gen_ai.usage.cost` is
// accepted as a courtesy for tools that emit it, but it is not part of the
// OpenLLMetry standard (see docs/llm-semantics.md).
const LLM_COST_USD: &str = "llm.cost_usd";
const GEN_AI_USAGE_COST: &str = "gen_ai.usage.cost";

// Prompt-caching counters. They matter to attribution because they decide
// what "prompt tokens" MEANS: Anthropic's `input_tokens` counts only the
// uncached remainder, so a conversation whose context is growing reports a
// prompt count that FALLS as the cache warms. Reading growth off the wrong
// field inverts the signal on the configuration long-running agents use.
//
// Both spellings are recognized, dotted first. OpenLLMetry moved these to
// dotted segment names to match OTel GenAI, and the underscore forms are what
// it emitted before that — so a store fed by a mixed fleet, or by anything
// pinned to an older exporter, holds both. Recognizing only one is not a
// cosmetic miss here: it silently returns the detector to reading the
// uncached remainder, which is the exact failure the field exists to avoid.
const GEN_AI_CACHE_READ_TOKENS: &str = "gen_ai.usage.cache_read.input_tokens";
const GEN_AI_CACHE_READ_TOKENS_LEGACY: &str = "gen_ai.usage.cache_read_input_tokens";
const GEN_AI_CACHE_CREATION_TOKENS: &str = "gen_ai.usage.cache_creation.input_tokens";
const GEN_AI_CACHE_CREATION_TOKENS_LEGACY: &str = "gen_ai.usage.cache_creation_input_tokens";

// -- Native Traza shorthand ------------------------------------------------
const LLM_MODEL: &str = "llm.model";
const LLM_PROMPT_TOKENS: &str = "llm.prompt_tokens";
const LLM_COMPLETION_TOKENS: &str = "llm.completion_tokens";
const LLM_TOTAL_TOKENS: &str = "llm.total_tokens";
const SESSION_ID: &str = "session.id";
// Native content carriers: an attribute of this name, or an event of this name
// whose `content` attribute holds the text (the shape docs/llm-semantics.md
// documents and `seed` emits).
const LLM_PROMPT: &str = "llm.prompt";
const LLM_COMPLETION: &str = "llm.completion";

// -- OpenInference (Arize Phoenix) -----------------------------------------
// OpenInference names the same facts under `llm.*` keys that happen not to
// collide with the native shorthand. Every one of them ranks AFTER the keys
// recognized before OpenInference was, so adding the vocabulary changed no
// span's existing answer — it only filled blanks.
//
// `llm.provider` is OpenInference's hosting provider (`openai`, `azure`,
// `aws`) and `llm.system` its model family (`openai`, `anthropic`); the
// provider ranks first because that is what `gen_ai.provider.name` means.
// Neither spelling of a host matches the provider list in
// `LlmFacts::context_tokens`, which therefore declines to sum cache counters
// for them rather than guess — the honest answer for an unknown convention.
//
// OpenInference's cache counters (`llm.token_count.prompt_details.*`) are NOT
// recognized: its prompt count already INCLUDES them, and folding them into
// the cache fields would make `context_tokens` double-count exactly the way
// that function exists to prevent.
const OI_SPAN_KIND: &str = "openinference.span.kind";
const OI_MODEL_NAME: &str = "llm.model_name";
const OI_PROVIDER: &str = "llm.provider";
const OI_SYSTEM: &str = "llm.system";
const OI_PROMPT_TOKENS: &str = "llm.token_count.prompt";
const OI_COMPLETION_TOKENS: &str = "llm.token_count.completion";
const OI_TOTAL_TOKENS: &str = "llm.token_count.total";
const OI_TOOL_NAME: &str = "tool.name";
const OI_AGENT_NAME: &str = "agent.name";
const OI_INPUT_VALUE: &str = "input.value";
const OI_OUTPUT_VALUE: &str = "output.value";
const OI_OUTPUT_MIME_TYPE: &str = "output.mime_type";
const OI_INPUT_MESSAGES_PREFIX: &str = "llm.input_messages.";
const OI_OUTPUT_MESSAGES_PREFIX: &str = "llm.output_messages.";

// -- Step identity: tools, agents, and a step's input and output -----------
const GEN_AI_TOOL_NAME: &str = "gen_ai.tool.name";
const GEN_AI_TOOL_CALL_ID: &str = "gen_ai.tool.call.id";
const GEN_AI_TOOL_CALL_ARGUMENTS: &str = "gen_ai.tool.call.arguments";
const GEN_AI_TOOL_CALL_RESULT: &str = "gen_ai.tool.call.result";
const GEN_AI_AGENT_NAME: &str = "gen_ai.agent.name";
const GEN_AI_INPUT_MESSAGES: &str = "gen_ai.input.messages";
const GEN_AI_OUTPUT_MESSAGES: &str = "gen_ai.output.messages";
const GEN_AI_SYSTEM_INSTRUCTIONS: &str = "gen_ai.system_instructions";
// The OpenLLMetry indexed layout, `gen_ai.prompt.{i}.{field}`. The trailing
// dot is load-bearing: it is what keeps `gen_ai.prompt_tokens`-style keys out.
const GEN_AI_PROMPT_PREFIX: &str = "gen_ai.prompt.";
const GEN_AI_COMPLETION_PREFIX: &str = "gen_ai.completion.";
const TRACELOOP_ENTITY_NAME: &str = "traceloop.entity.name";
const TRACELOOP_ENTITY_INPUT: &str = "traceloop.entity.input";
const TRACELOOP_ENTITY_OUTPUT: &str = "traceloop.entity.output";

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
    /// Provider (`gen_ai.provider.name`, else the deprecated `gen_ai.system`,
    /// else OpenInference `llm.provider` / `llm.system`; for example
    /// `openai`, `anthropic`, `aws.bedrock`).
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
    /// Cost in USD: metered from the span, or derived by [`Self::priced`].
    /// [`Self::cost_derived`] says which.
    pub cost_usd: Option<f64>,
    /// Whether [`Self::cost_usd`] was computed from a pricing table rather
    /// than reported by the span. Always false as extracted; only
    /// [`Self::priced`] sets it.
    pub cost_derived: bool,
    /// Prompt tokens served from a provider's cache, if reported.
    pub cache_read_tokens: Option<u64>,
    /// Prompt tokens written into a provider's cache, if reported.
    pub cache_creation_tokens: Option<u64>,
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

    /// Fills in a cost from `pricing` when the span did not meter one.
    ///
    /// A metered cost is never overwritten. The span's own number is a
    /// measurement of what was actually charged; the table's is arithmetic at
    /// list price, and preferring the second would be replacing evidence with
    /// an estimate. See [`crate::pricing`].
    pub fn priced(mut self, pricing: &crate::pricing::Pricing) -> Self {
        if self.cost_usd.is_some() || pricing.is_empty() {
            return self;
        }
        if let Some(model) = self.model.as_deref() {
            if let Some(cost) = pricing.cost(model, self.prompt_tokens, self.completion_tokens) {
                self.cost_usd = Some(cost);
                self.cost_derived = true;
            }
        }
        self
    }

    /// How much context this call actually carried, for the calls where that
    /// can be said honestly — `None` when it cannot.
    ///
    /// This is the measure a runaway is read from, and it is deliberately not
    /// [`Self::prompt_tokens`], because the two providers disagree about what
    /// that field counts once prompt caching is on:
    ///
    /// - **Anthropic** reports `input_tokens` as the UNCACHED remainder, with
    ///   the cached part in `cache_read.input_tokens` /
    ///   `cache_creation.input_tokens` (the underscore spellings are accepted
    ///   as legacy aliases). The context is the sum, and reading
    ///   `input_tokens` alone shows a growing conversation SHRINKING as its
    ///   cache warms.
    /// - **OpenAI** reports `prompt_tokens` inclusive of its cached subset, so
    ///   adding the cache counters double-counts.
    ///
    /// So the sum is taken only when the provider is known to report the
    /// exclusive form. When cache counters are present under a provider whose
    /// convention is not known, this returns `None` — the honest answer, which
    /// a caller must render as "cannot tell" rather than as a number. Without
    /// cache counters the question does not arise and prompt tokens stand.
    pub fn context_tokens(&self) -> Option<u64> {
        let cached = self
            .cache_read_tokens
            .unwrap_or(0)
            .saturating_add(self.cache_creation_tokens.unwrap_or(0));
        let prompt = self.prompt_tokens?;
        if cached == 0 {
            return Some(prompt);
        }
        match self.provider.as_deref() {
            // The providers whose `input_tokens` excludes the cached part.
            Some("anthropic") | Some("aws.bedrock") | Some("gcp.vertex_ai") => {
                Some(prompt.saturating_add(cached))
            }
            // Inclusive, so the prompt count already IS the context.
            Some("openai") | Some("azure.ai.openai") => Some(prompt),
            _ => None,
        }
    }

    /// Whether this call reports cache counters under a provider whose
    /// arithmetic is not known, so [`Self::context_tokens`] declines to answer.
    ///
    /// The distinction matters to any caller reading a TREND: a span that
    /// reported nothing is a gap in the series, while a span that reported
    /// something uninterpretable is a reason to distrust the series. Both come
    /// back from `context_tokens` as `None`, so the two are told apart here
    /// rather than by guessing which `None` is which.
    pub fn context_is_ambiguous(&self) -> bool {
        let cached = self
            .cache_read_tokens
            .unwrap_or(0)
            .saturating_add(self.cache_creation_tokens.unwrap_or(0));
        self.prompt_tokens.is_some() && cached > 0 && self.context_tokens().is_none()
    }
}

/// Extracts [`LlmFacts`] from a span's attribute map.
///
/// Each fact is the first key present, in this order:
///
/// | Fact | Keys (first present wins) |
/// |---|---|
/// | model | `gen_ai.response.model` → `gen_ai.request.model` → `llm.model` → `llm.model_name` |
/// | provider | `gen_ai.provider.name` → `gen_ai.system` → `llm.provider` → `llm.system` |
/// | prompt tokens | `gen_ai.usage.input_tokens` → `gen_ai.usage.prompt_tokens` → `llm.prompt_tokens` → `llm.token_count.prompt` |
/// | completion tokens | `gen_ai.usage.output_tokens` → `gen_ai.usage.completion_tokens` → `llm.completion_tokens` → `llm.token_count.completion` |
/// | total tokens | `llm.usage.total_tokens` → `gen_ai.usage.total_tokens` → `llm.total_tokens` → `llm.token_count.total` |
/// | cost | `llm.cost_usd` → `gen_ai.usage.cost` |
/// | cache read | `gen_ai.usage.cache_read.input_tokens` → `gen_ai.usage.cache_read_input_tokens` |
/// | cache creation | `gen_ai.usage.cache_creation.input_tokens` → `gen_ai.usage.cache_creation_input_tokens` |
/// | session | `session.id` → `gen_ai.conversation.id` → `traceloop.association.properties.session_id` → `traceloop.association.properties.chat_id` |
///
/// A span is an LLM call (`is_llm`) when it carries any model, provider or
/// token count, `gen_ai.operation.name` (any value), `llm.request.type`,
/// `traceloop.span.kind == "llm"`, or `openinference.span.kind` equal to
/// `LLM` (ASCII case-insensitive).
pub fn facts(attributes: &Map<String, Value>) -> LlmFacts {
    let model = first_str(
        attributes,
        &[
            GEN_AI_RESPONSE_MODEL,
            GEN_AI_REQUEST_MODEL,
            LLM_MODEL,
            OI_MODEL_NAME,
        ],
    );
    let provider = first_str(
        attributes,
        &[GEN_AI_PROVIDER_NAME, GEN_AI_SYSTEM, OI_PROVIDER, OI_SYSTEM],
    );
    let prompt_tokens = first_u64(
        attributes,
        &[
            GEN_AI_USAGE_INPUT_TOKENS,
            GEN_AI_USAGE_PROMPT_TOKENS,
            LLM_PROMPT_TOKENS,
            OI_PROMPT_TOKENS,
        ],
    );
    let completion_tokens = first_u64(
        attributes,
        &[
            GEN_AI_USAGE_OUTPUT_TOKENS,
            GEN_AI_USAGE_COMPLETION_TOKENS,
            LLM_COMPLETION_TOKENS,
            OI_COMPLETION_TOKENS,
        ],
    );
    let total_tokens = first_u64(
        attributes,
        &[
            LLM_USAGE_TOTAL_TOKENS,
            GEN_AI_USAGE_TOTAL_TOKENS,
            LLM_TOTAL_TOKENS,
            OI_TOTAL_TOKENS,
        ],
    );
    let cost_usd = first_f64(attributes, &[LLM_COST_USD, GEN_AI_USAGE_COST]);
    let cache_read_tokens = first_u64(
        attributes,
        &[GEN_AI_CACHE_READ_TOKENS, GEN_AI_CACHE_READ_TOKENS_LEGACY],
    );
    let cache_creation_tokens = first_u64(
        attributes,
        &[
            GEN_AI_CACHE_CREATION_TOKENS,
            GEN_AI_CACHE_CREATION_TOKENS_LEGACY,
        ],
    );
    let (session, session_key) = SESSION_KEYS
        .iter()
        .find_map(|key| attr_str(attributes, key).map(|value| (value, *key)))
        .map_or((None, None), |(value, key)| (Some(value), Some(key)));
    let is_llm = model.is_some()
        || provider.is_some()
        || prompt_tokens.is_some()
        || completion_tokens.is_some()
        || total_tokens.is_some()
        || attributes.contains_key(GEN_AI_OPERATION_NAME)
        || attributes.contains_key(LLM_REQUEST_TYPE)
        || attr_str(attributes, TRACELOOP_SPAN_KIND).as_deref() == Some("llm")
        || attr_str(attributes, OI_SPAN_KIND).is_some_and(|kind| kind.eq_ignore_ascii_case("llm"));
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
        // Extraction reports only what the span said. Pricing is applied by
        // `LlmFacts::priced`, at the sites that hold the store's table.
        cost_derived: false,
        cache_read_tokens,
        cache_creation_tokens,
    }
}

/// Attribute keys declaring how a session ENDED, in precedence order.
///
/// Nothing in OpenTelemetry GenAI defines a session outcome — the conventions
/// describe individual model calls, not the agent run around them — so this is
/// a Traza extension and is labelled one, exactly as `llm.cost_usd` is. That
/// has a consequence worth stating where the constant lives: **no existing
/// pipeline emits these keys**, so a declared outcome is a bonus and never a
/// precondition. Everything that asks "did this run succeed" has to answer
/// from ordinary telemetry when no one declared anything (see
/// [`crate::attribution`]).
pub(crate) const OUTCOME_KEYS: [&str; 2] = [
    "session.outcome",
    "traceloop.association.properties.outcome",
];

/// Attribute keys carrying the goal a session was pursuing.
pub(crate) const GOAL_KEYS: [&str; 2] = ["session.goal", "traceloop.association.properties.goal"];

/// The declared outcome of a span, if it carries one.
pub(crate) fn outcome(attributes: &Map<String, Value>) -> Option<String> {
    first_str(attributes, &OUTCOME_KEYS)
}

/// The declared goal of a span, if it carries one.
pub(crate) fn goal(attributes: &Map<String, Value>) -> Option<String> {
    first_str(attributes, &GOAL_KEYS)
}

// ------------------------------------------------------------ run identity

/// Prefix of a run id that names a trace rather than a session.
pub const TRACE_RUN_PREFIX: &str = "trace:";

/// The run a span belongs to: its session id when it carries one, under the
/// same precedence as [`LlmFacts::session`] (`session.id` →
/// `gen_ai.conversation.id` → `traceloop.association.properties.session_id`
/// → `traceloop.association.properties.chat_id`), else `trace:<trace_id>`.
///
/// A run is that id WITHIN A TENANT. The session form makes a conversation
/// that spans many traces one run; the trace form keeps a span with no session
/// key from being orphaned — an uninstrumented trace is still a run of its own.
///
/// Identity is per span, exactly as session membership is: a trace whose root
/// carries `session.id` but whose children do not puts the children in
/// `trace:<id>`, not in the session. And a session id that itself begins with
/// `trace:` is indistinguishable from the trace form; [`run_trace_id`] reads it
/// as a trace.
pub fn run_id(attributes: &Map<String, Value>, trace_id: &str) -> String {
    SESSION_KEYS
        .iter()
        .find_map(|key| attr_str(attributes, key))
        .unwrap_or_else(|| format!("{TRACE_RUN_PREFIX}{trace_id}"))
}

/// The trace id a run id names, when it is the `trace:<id>` form; `None` for
/// a session run.
pub fn run_trace_id(run_id: &str) -> Option<&str> {
    run_id.strip_prefix(TRACE_RUN_PREFIX)
}

// -------------------------------------------------------------- step facts

/// What kind of work a span records, as far as its run is concerned.
///
/// Serialized lowercase (`"llm"`, `"tool"`, …), which is also
/// [`StepKind::as_str`] and the prefix of a [`StepFacts::fingerprint`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepKind {
    /// A model call: chat, text completion, content generation.
    Llm,
    /// A tool execution.
    Tool,
    /// An agent invocation or creation.
    Agent,
    /// A retrieval (OpenInference `RETRIEVER`).
    Retrieval,
    /// An embeddings call.
    Embedding,
    /// Anything else: workflow, task and chain wrappers, HTTP, database work.
    #[default]
    Other,
}

impl StepKind {
    /// The lowercase wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Llm => "llm",
            Self::Tool => "tool",
            Self::Agent => "agent",
            Self::Retrieval => "retrieval",
            Self::Embedding => "embedding",
            Self::Other => "other",
        }
    }

    /// Whether a span of this kind is a STEP of its run — a model call, a tool
    /// call, or an agent invocation. These are what a run's step count, its
    /// repeat detection and its tape are made of; retrievals, embeddings and
    /// wrappers are context around steps, not steps.
    pub fn is_step(self) -> bool {
        matches!(self, Self::Llm | Self::Tool | Self::Agent)
    }
}

impl fmt::Display for StepKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A span read as one step of a run: what kind of step it is, what it called,
/// what went in and what came out. See [`step_facts`] for every key source.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StepFacts {
    /// The step kind.
    pub kind: StepKind,
    /// The tool's name, when the span names one.
    pub tool: Option<String>,
    /// The agent's name, when the span names one.
    pub agent: Option<String>,
    /// The tool call's id (`gen_ai.tool.call.id`), which ties a tool span to
    /// the model output that requested it.
    pub tool_call_id: Option<String>,
    /// The step's input as stored. Never resolved: an offloaded value is its
    /// `{"$payload": "sha256/…", …}` reference object.
    pub request: Option<Value>,
    /// The step's output as stored, under the same rules as
    /// [`Self::request`].
    pub response: Option<Value>,
    /// Lowercase hex SHA-256 identifying [`Self::request`]; see
    /// [`step_facts`] for how it is computed.
    pub request_digest: Option<String>,
    /// Lowercase hex SHA-256 identifying [`Self::response`].
    pub response_digest: Option<String>,
    /// The step's signature plus a request hash:
    /// `"{kind}:{name}"`, then `"#"` and the first 16 hex characters of
    /// [`Self::request_digest`] when there is a request. Two spans with the
    /// same fingerprint made the same call with the same input — what a loop
    /// looks like.
    pub fingerprint: String,
}

/// Hex characters of the request digest a fingerprint carries: 64 bits, so a
/// collision between two different inputs of the same step is not a practical
/// concern, while the fingerprint stays short enough to read in a reason line.
const FINGERPRINT_DIGEST_CHARS: usize = 16;

impl StepFacts {
    /// `"{kind}:{name}"` — the fingerprint without its request hash. Steps are
    /// aligned on this (the same call with possibly different input).
    pub fn signature(&self) -> &str {
        match self.request_digest {
            // The suffix is exactly '#' plus FINGERPRINT_DIGEST_CHARS hex
            // characters, so it is cut by length rather than by searching for
            // '#', which a tool or span name may legitimately contain.
            Some(_) => &self.fingerprint[..self.fingerprint.len() - FINGERPRINT_DIGEST_CHARS - 1],
            None => &self.fingerprint,
        }
    }
}

/// Extracts [`StepFacts`] from a span.
///
/// Takes the whole span rather than its attribute map: the fingerprint falls
/// back to the span NAME, and Traza's native prompt/completion carriers are
/// EVENTS. It is still a pure function of the span.
///
/// **Kind** — the first source that yields a recognized value wins; an
/// unrecognized value falls through to the next source. Values compare ASCII
/// case-insensitively.
///
/// | Source | Values |
/// |---|---|
/// | `gen_ai.operation.name` | `chat`, `text_completion`, `generate_content` → llm; `execute_tool` → tool; `invoke_agent`, `create_agent` → agent; `embeddings` → embedding |
/// | `openinference.span.kind` | `LLM` → llm; `TOOL` → tool; `AGENT` → agent; `RETRIEVER` → retrieval; `EMBEDDING` → embedding; `CHAIN` → other |
/// | `traceloop.span.kind` | `llm` → llm; `tool` → tool; `agent` → agent; `workflow`, `task` → other |
/// | [`facts`] says `is_llm` | llm |
/// | a tool-name key is present (`gen_ai.tool.name`, `tool.name`) | tool |
/// | otherwise | other |
///
/// Because `is_llm` is true for ANY `gen_ai.operation.name`, an operation
/// name this table does not know lands at llm by the fourth row unless an
/// OpenInference or Traceloop kind decided first.
///
/// **Names and ids** — first present wins:
///
/// | Fact | Keys |
/// |---|---|
/// | tool | `gen_ai.tool.name` → `tool.name` → `traceloop.entity.name` (only when `traceloop.span.kind` is `tool`) |
/// | agent | `gen_ai.agent.name` → `traceloop.entity.name` (only when `traceloop.span.kind` is `agent` or `workflow`) → `agent.name` |
/// | tool call id | `gen_ai.tool.call.id` |
///
/// **Request and response** — first present wins, by kind:
///
/// | Kind | Request | Response |
/// |---|---|---|
/// | llm | `gen_ai.input.messages` → indexed `gen_ai.prompt.{i}.*` → indexed `llm.input_messages.{i}.message.*` → `input.value` → `llm.prompt` attribute → `llm.prompt` events | `gen_ai.output.messages` → indexed `gen_ai.completion.{i}.*` → indexed `llm.output_messages.{i}.message.*` → `output.value` → `llm.completion` attribute → `llm.completion` events |
/// | tool | `gen_ai.tool.call.arguments` → `traceloop.entity.input` → `input.value` | `gen_ai.tool.call.result` → `traceloop.entity.output` → `output.value` |
/// | any other | `traceloop.entity.input` → `input.value` | `traceloop.entity.output` → `output.value` |
///
/// An absent, `null` or empty-string attribute is not a source. A string
/// whose text is a JSON object or array is returned parsed; any other string —
/// including one that happens to read `42` or `true` — stays the text it was.
///
/// Indexed layouts are reassembled into a list ordered by numeric index (`10`
/// after `9`, an index with a leading zero ignored), one object per message
/// carrying whichever of `role`, `content`, `finish_reason`, `tool_call_id`
/// and `tool_calls` (a list of `{id, name, arguments}`) the span recorded:
/// OpenLLMetry's `{i}.tool_calls.{j}.{id,name,arguments}` and OpenInference's
/// `{i}.message.tool_calls.{j}.tool_call.{id,function.name,function.arguments}`
/// fold to the same shape. Native events become `[{role?, content}]` in event
/// order.
///
/// `gen_ai.system_instructions`, when present on an llm step, becomes a first
/// message — `{"role": "system", "parts": […]}` when it is a list of parts (its
/// OTel shape), `{"role": "system", "content": …}` otherwise — prepended to a
/// message list, or forming one when there is no other request. A request that
/// is not a list (an offloaded reference, a bare prompt string) becomes the
/// second element of a two-element list after that system message: neither
/// carrier is dropped, so the digest covers both, and once an offloaded list
/// is resolved the two combine exactly as they would have inline (see
/// [`crate::tape`]).
///
/// **Digests** are the SHA-256 of the value's canonical JSON (object keys
/// sorted bytewise, no whitespace), as lowercase hex — so the same messages
/// sent with different key order or spacing digest identically. A request that
/// is wholly a `$payload` reference digests to the content address it names
/// (`sha256/<hex>` → `<hex>`), without reading the payload. That address is
/// the hash of the original attribute TEXT, so an offloaded copy and an inline
/// copy of the same input digest differently; compare digests between steps
/// stored under the same offload threshold.
///
/// **Fingerprint** — `"{kind}:{name}"`, where name is the tool name for a tool
/// step, the model ([`facts`]) for an llm step, the agent name for an agent
/// step, and the span name otherwise or when that fact is absent; then `#` and
/// the first 16 hex characters of the request digest when there is a request.
pub fn step_facts(span: &Span) -> StepFacts {
    step_facts_with(span, &facts(&span.attributes))
}

/// [`step_facts`] reusing facts the caller already extracted, so a caller that
/// needs both (and prices them) extracts once. Pricing cannot change a step
/// fact: only `is_llm` and the model are read.
pub(crate) fn step_facts_with(span: &Span, llm: &LlmFacts) -> StepFacts {
    step_facts_detailed(span, llm).0
}

/// An llm request's two carriers as stored — its message list (or whatever
/// stood in for one) and `gen_ai.system_instructions` — kept apart because at
/// least one of them is an offloaded `$payload` reference, so how they
/// combine depends on content that has not been read yet.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RequestCarriers {
    /// The message list as stored, if the span recorded one.
    pub(crate) messages: Option<Value>,
    /// The system instructions as stored.
    pub(crate) system: Value,
}

/// Where an llm step's response was read from, which decides how a replay
/// decodes it. An object-shaped value means different things by source:
/// answer TEXT that happens to be JSON must come back as that text, while a
/// provider's response body must be unwrapped. Guessing from the object's
/// keys cannot tell `{"content": "hi", "count": 2}` the answer from
/// `{"content": "hi"}` the message.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ResponseSource {
    /// A message list: `gen_ai.output.messages`, an indexed layout, or native
    /// events.
    #[default]
    Messages,
    /// OpenInference's `output.value` with no text MIME type: the provider's
    /// response body as the instrumentation serialized it.
    Body,
    /// The answer's text: the `llm.completion` attribute, or an
    /// `output.value` whose `output.mime_type` says it is not JSON.
    Text,
}

/// What a tape step needs from its span beyond [`StepFacts`].
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct StepExtras {
    /// llm steps: the request's carriers, when combining them has to wait
    /// for payload resolution ([`RequestCarriers`]).
    pub(crate) request_carriers: Option<RequestCarriers>,
    /// llm steps: which source the response came from.
    pub(crate) response_source: ResponseSource,
    /// Tool steps with no response: the value of the first response
    /// attribute the span carried, when it carried one that reads as absent
    /// (`null` or the empty string). It tells a result recorded as empty
    /// from one that was never captured — the response is `None` for both.
    pub(crate) empty_response: Option<Value>,
}

/// [`step_facts_with`] plus the [`StepExtras`] a tape step keeps, extracted
/// together so neither repeats the other's work.
pub(crate) fn step_facts_detailed(span: &Span, llm: &LlmFacts) -> (StepFacts, StepExtras) {
    let attributes = &span.attributes;
    let traceloop_kind =
        attr_str(attributes, TRACELOOP_SPAN_KIND).map(|kind| kind.to_ascii_lowercase());
    let kind = step_kind(attributes, traceloop_kind.as_deref(), llm);

    let tool = step_tool(attributes, traceloop_kind.as_deref());
    let agent = step_agent(attributes, traceloop_kind.as_deref());
    let tool_call_id = attr_str(attributes, GEN_AI_TOOL_CALL_ID);

    let mut extras = StepExtras::default();
    let request = match kind {
        StepKind::Llm => {
            let (messages, system) = llm_request_carriers(span);
            // Offload replaces a whole attribute value, so a carrier is
            // either inline or one reference — never inline with references
            // inside it. Inline carriers are combined for good right here.
            if let Some(system) = &system {
                if is_payload_reference(system)
                    || messages.as_ref().is_some_and(is_payload_reference)
                {
                    extras.request_carriers = Some(RequestCarriers {
                        messages: messages.clone(),
                        system: system.clone(),
                    });
                }
            }
            with_system_instructions(messages, system)
        }
        _ => step_request(span, kind),
    };
    let response = match kind {
        StepKind::Llm => llm_response_sourced(span).map(|(response, source)| {
            extras.response_source = source;
            response
        }),
        StepKind::Tool => {
            let response = first_payload(attributes, TOOL_RESPONSE_KEYS);
            if response.is_none() {
                extras.empty_response = TOOL_RESPONSE_KEYS
                    .iter()
                    .find_map(|key| attributes.get(*key))
                    .cloned();
            }
            response
        }
        _ => first_payload(attributes, &[TRACELOOP_ENTITY_OUTPUT, OI_OUTPUT_VALUE]),
    };
    let request_digest = request.as_ref().map(value_digest);
    let response_digest = response.as_ref().map(value_digest);
    let fingerprint = step_fingerprint(
        span,
        kind,
        tool.as_deref(),
        agent.as_deref(),
        llm,
        request_digest.as_deref(),
    );

    let facts = StepFacts {
        kind,
        tool,
        agent,
        tool_call_id,
        request,
        response,
        request_digest,
        response_digest,
        fingerprint,
    };
    (facts, extras)
}

/// A tool step's response keys, in precedence order.
const TOOL_RESPONSE_KEYS: &[&str] = &[
    GEN_AI_TOOL_CALL_RESULT,
    TRACELOOP_ENTITY_OUTPUT,
    OI_OUTPUT_VALUE,
];

/// The fingerprint [`step_facts_with`] would report, computed from the
/// request alone: the response is neither extracted nor digested, since the
/// fingerprint does not cover it. For a caller that fingerprints every
/// acknowledged step on the ingest path (the run guard), where a second
/// digest per step would be pure waste. Built from the same helpers as
/// [`step_facts_with`], so the two cannot disagree.
pub(crate) fn step_fingerprint_with(span: &Span, llm: &LlmFacts) -> String {
    let attributes = &span.attributes;
    let traceloop_kind =
        attr_str(attributes, TRACELOOP_SPAN_KIND).map(|kind| kind.to_ascii_lowercase());
    let kind = step_kind(attributes, traceloop_kind.as_deref(), llm);
    // Only the name the kind uses is looked up.
    let tool = match kind {
        StepKind::Tool => step_tool(attributes, traceloop_kind.as_deref()),
        _ => None,
    };
    let agent = match kind {
        StepKind::Agent => step_agent(attributes, traceloop_kind.as_deref()),
        _ => None,
    };
    let request_digest = step_request(span, kind).as_ref().map(value_digest);
    step_fingerprint(
        span,
        kind,
        tool.as_deref(),
        agent.as_deref(),
        llm,
        request_digest.as_deref(),
    )
}

/// The tool name, per the names table on [`step_facts`].
fn step_tool(attributes: &Map<String, Value>, traceloop_kind: Option<&str>) -> Option<String> {
    first_str(attributes, &[GEN_AI_TOOL_NAME, OI_TOOL_NAME]).or_else(|| match traceloop_kind {
        Some("tool") => attr_str(attributes, TRACELOOP_ENTITY_NAME),
        _ => None,
    })
}

/// The agent name, per the names table on [`step_facts`].
fn step_agent(attributes: &Map<String, Value>, traceloop_kind: Option<&str>) -> Option<String> {
    attr_str(attributes, GEN_AI_AGENT_NAME)
        .or_else(|| match traceloop_kind {
            Some("agent" | "workflow") => attr_str(attributes, TRACELOOP_ENTITY_NAME),
            _ => None,
        })
        .or_else(|| attr_str(attributes, OI_AGENT_NAME))
}

/// The step's request, per the request table on [`step_facts`].
fn step_request(span: &Span, kind: StepKind) -> Option<Value> {
    match kind {
        StepKind::Llm => llm_request(span),
        StepKind::Tool => first_payload(
            &span.attributes,
            &[
                GEN_AI_TOOL_CALL_ARGUMENTS,
                TRACELOOP_ENTITY_INPUT,
                OI_INPUT_VALUE,
            ],
        ),
        _ => first_payload(&span.attributes, &[TRACELOOP_ENTITY_INPUT, OI_INPUT_VALUE]),
    }
}

/// `"{kind}:{name}"`, then `#` and the request digest's prefix when there is
/// a request; see [`step_facts`].
fn step_fingerprint(
    span: &Span,
    kind: StepKind,
    tool: Option<&str>,
    agent: Option<&str>,
    llm: &LlmFacts,
    request_digest: Option<&str>,
) -> String {
    let name = match kind {
        StepKind::Tool => tool,
        StepKind::Llm => llm.model.as_deref(),
        StepKind::Agent => agent,
        _ => None,
    }
    .unwrap_or(&span.name);
    let mut fingerprint = format!("{kind}:{name}");
    if let Some(digest) = request_digest {
        // Every digest is 64 hex characters (a SHA-256 or a validated content
        // address), so the slice is in bounds and on a char boundary.
        fingerprint.push('#');
        fingerprint.push_str(&digest[..FINGERPRINT_DIGEST_CHARS]);
    }
    fingerprint
}

/// The kind [`step_facts_with`] would report, without extracting or
/// digesting the step's request and response — for a caller that needs the
/// rest of the facts only for some kinds (the run guard fingerprints steps
/// alone).
pub(crate) fn step_kind_with(span: &Span, llm: &LlmFacts) -> StepKind {
    let traceloop_kind =
        attr_str(&span.attributes, TRACELOOP_SPAN_KIND).map(|kind| kind.to_ascii_lowercase());
    step_kind(&span.attributes, traceloop_kind.as_deref(), llm)
}

/// The kind precedence documented on [`step_facts`].
fn step_kind(
    attributes: &Map<String, Value>,
    traceloop_kind: Option<&str>,
    llm: &LlmFacts,
) -> StepKind {
    let operation =
        attr_str(attributes, GEN_AI_OPERATION_NAME).and_then(|operation| {
            match operation.to_ascii_lowercase().as_str() {
                "chat" | "text_completion" | "generate_content" => Some(StepKind::Llm),
                "execute_tool" => Some(StepKind::Tool),
                "invoke_agent" | "create_agent" => Some(StepKind::Agent),
                "embeddings" => Some(StepKind::Embedding),
                _ => None,
            }
        });
    let openinference = || {
        attr_str(attributes, OI_SPAN_KIND).and_then(|kind| {
            match kind.to_ascii_uppercase().as_str() {
                "LLM" => Some(StepKind::Llm),
                "TOOL" => Some(StepKind::Tool),
                "AGENT" => Some(StepKind::Agent),
                "RETRIEVER" => Some(StepKind::Retrieval),
                "EMBEDDING" => Some(StepKind::Embedding),
                "CHAIN" => Some(StepKind::Other),
                _ => None,
            }
        })
    };
    let traceloop = || match traceloop_kind {
        Some("llm") => Some(StepKind::Llm),
        Some("tool") => Some(StepKind::Tool),
        Some("agent") => Some(StepKind::Agent),
        Some("workflow" | "task") => Some(StepKind::Other),
        _ => None,
    };
    operation
        .or_else(openinference)
        .or_else(traceloop)
        .unwrap_or_else(|| {
            if llm.is_llm {
                StepKind::Llm
            } else if first_str(attributes, &[GEN_AI_TOOL_NAME, OI_TOOL_NAME]).is_some() {
                StepKind::Tool
            } else {
                StepKind::Other
            }
        })
}

/// An llm step's input, per the request column of [`step_facts`].
fn llm_request(span: &Span) -> Option<Value> {
    let (messages, system) = llm_request_carriers(span);
    with_system_instructions(messages, system)
}

/// An llm step's two request carriers as stored: the message list (or what
/// stands in for one) by the request precedence, and
/// `gen_ai.system_instructions`.
fn llm_request_carriers(span: &Span) -> (Option<Value>, Option<Value>) {
    let attributes = &span.attributes;
    let messages = attributes
        .get(GEN_AI_INPUT_MESSAGES)
        .and_then(payload_value)
        .or_else(|| indexed_messages(attributes, GEN_AI_PROMPT_PREFIX, Layout::OpenLlmetry))
        .or_else(|| indexed_messages(attributes, OI_INPUT_MESSAGES_PREFIX, Layout::OpenInference))
        .or_else(|| first_payload(attributes, &[OI_INPUT_VALUE, LLM_PROMPT]))
        .or_else(|| event_messages(span, LLM_PROMPT));
    let system = attributes
        .get(GEN_AI_SYSTEM_INSTRUCTIONS)
        .and_then(payload_value);
    (messages, system)
}

/// An llm step's output, per the response column of [`step_facts`], and
/// which kind of source it came from.
fn llm_response_sourced(span: &Span) -> Option<(Value, ResponseSource)> {
    let attributes = &span.attributes;
    let messages = |value: Value| (value, ResponseSource::Messages);
    attributes
        .get(GEN_AI_OUTPUT_MESSAGES)
        .and_then(payload_value)
        .map(messages)
        .or_else(|| {
            indexed_messages(attributes, GEN_AI_COMPLETION_PREFIX, Layout::OpenLlmetry)
                .map(messages)
        })
        .or_else(|| {
            indexed_messages(attributes, OI_OUTPUT_MESSAGES_PREFIX, Layout::OpenInference)
                .map(messages)
        })
        .or_else(|| {
            // OpenInference names the value's type: JSON is the provider's
            // response body, anything else (`text/plain`) is the answer.
            let source = match attr_str(attributes, OI_OUTPUT_MIME_TYPE) {
                Some(mime) if !mime.to_ascii_lowercase().contains("json") => ResponseSource::Text,
                _ => ResponseSource::Body,
            };
            attributes
                .get(OI_OUTPUT_VALUE)
                .and_then(payload_value)
                .map(|value| (value, source))
        })
        .or_else(|| {
            attributes
                .get(LLM_COMPLETION)
                .and_then(payload_value)
                .map(|value| (value, ResponseSource::Text))
        })
        .or_else(|| event_messages(span, LLM_COMPLETION).map(messages))
}

/// Folds `gen_ai.system_instructions` into an llm request as its first
/// message, per [`step_facts`]. Applied to the carriers as stored, and again
/// to them once resolved when either was offloaded ([`RequestCarriers`]), so
/// a resolved request has the shape it would have had inline.
pub(crate) fn with_system_instructions(
    messages: Option<Value>,
    system: Option<Value>,
) -> Option<Value> {
    let Some(system) = system else {
        return messages;
    };
    let mut message = Map::new();
    message.insert("role".to_owned(), Value::String("system".to_owned()));
    match system {
        // OTel's shape for system instructions is a list of parts, the same
        // `parts` a message carries, so it becomes exactly that message.
        parts @ Value::Array(_) => message.insert("parts".to_owned(), parts),
        content => message.insert("content".to_owned(), content),
    };
    let list = match messages {
        None => vec![Value::Object(message)],
        Some(Value::Array(mut list)) => {
            list.insert(0, Value::Object(message));
            list
        }
        // Not a message list: an offloaded reference not yet read, or a bare
        // prompt. Dropping the system instructions would lose part of what
        // the model was given (and let two different requests digest alike),
        // and reshaping the request into messages would invent structure the
        // span never recorded — so it is kept whole, after them.
        Some(other) => vec![Value::Object(message), other],
    };
    Some(Value::Array(list))
}

/// Whether `value` is an offloaded `$payload` reference, as the resolver
/// reads one.
fn is_payload_reference(value: &Value) -> bool {
    value
        .as_object()
        .and_then(|object| object.get(crate::payload::PAYLOAD_KEY))
        .is_some_and(Value::is_string)
}

/// The first of `keys` that holds a step input or output.
fn first_payload(attributes: &Map<String, Value>, keys: &[&str]) -> Option<Value> {
    keys.iter()
        .find_map(|key| attributes.get(*key).and_then(payload_value))
}

/// An attribute value read as a step input or output: `null` and the empty
/// string are absent, and a string holding a JSON object or array is parsed.
fn payload_value(value: &Value) -> Option<Value> {
    match value {
        Value::Null => None,
        Value::String(text) if text.is_empty() => None,
        other => Some(field_value(other)),
    }
}

/// A value inside a step input or output, kept verbatim except that a string
/// holding a JSON object or array is parsed.
///
/// Only objects and arrays: a prompt that reads `42`, `true` or `null` is
/// text, and parsing it would both change what the step said and digest it as
/// a different type than the instrumentation recorded.
pub(crate) fn field_value(value: &Value) -> Value {
    if let Value::String(text) = value {
        let trimmed = text.trim_start();
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            if let Ok(parsed) = serde_json::from_str::<Value>(text) {
                return parsed;
            }
        }
    }
    value.clone()
}

/// Which convention's indexed message layout a prefix carries.
#[derive(Clone, Copy)]
enum Layout {
    /// `gen_ai.{prompt,completion}.{i}.{role,content,finish_reason,
    /// tool_call_id}` and `.{i}.tool_calls.{j}.{id,name,arguments}`.
    OpenLlmetry,
    /// `llm.{input,output}_messages.{i}.message.{role,content,tool_call_id}`
    /// and `.{i}.message.tool_calls.{j}.tool_call.{id,function.name,
    /// function.arguments}`.
    OpenInference,
}

/// Where one indexed attribute lands in a reassembled message.
enum MessageField {
    /// A field of the message itself.
    Message(&'static str),
    /// A field of the message's `j`th tool call.
    ToolCall(u64, &'static str),
}

/// One message being reassembled from indexed attributes.
#[derive(Default)]
struct IndexedMessage {
    fields: Map<String, Value>,
    tool_calls: BTreeMap<u64, Map<String, Value>>,
}

/// Reassembles `{prefix}{i}.{field}` attributes into a message list ordered by
/// `i`, or `None` when the span carries none.
fn indexed_messages(
    attributes: &Map<String, Value>,
    prefix: &str,
    layout: Layout,
) -> Option<Value> {
    let mut messages: BTreeMap<u64, IndexedMessage> = BTreeMap::new();
    for (key, value) in attributes {
        let Some((index, path)) = key
            .strip_prefix(prefix)
            .and_then(|rest| rest.split_once('.'))
        else {
            continue;
        };
        let (Some(index), Some(field)) = (parse_index(index), message_field(path, layout)) else {
            continue;
        };
        if value.is_null() {
            continue;
        }
        let message = messages.entry(index).or_default();
        match field {
            MessageField::Message(name) => {
                message.fields.insert(name.to_owned(), field_value(value));
            }
            MessageField::ToolCall(call, name) => {
                message
                    .tool_calls
                    .entry(call)
                    .or_default()
                    .insert(name.to_owned(), field_value(value));
            }
        }
    }
    if messages.is_empty() {
        return None;
    }
    let list = messages
        .into_values()
        .map(|message| {
            let mut fields = message.fields;
            if !message.tool_calls.is_empty() {
                let calls = message
                    .tool_calls
                    .into_values()
                    .map(Value::Object)
                    .collect();
                fields.insert("tool_calls".to_owned(), Value::Array(calls));
            }
            Value::Object(fields)
        })
        .collect();
    Some(Value::Array(list))
}

/// The message field an indexed attribute path (after `{prefix}{i}.`) names,
/// or `None` for a field Traza does not reassemble.
fn message_field(path: &str, layout: Layout) -> Option<MessageField> {
    let (path, call_fields): (&str, &[(&str, &'static str)]) = match layout {
        Layout::OpenLlmetry => (
            path,
            &[("id", "id"), ("name", "name"), ("arguments", "arguments")],
        ),
        Layout::OpenInference => (
            path.strip_prefix("message.")?,
            &[
                ("tool_call.id", "id"),
                ("tool_call.function.name", "name"),
                ("tool_call.function.arguments", "arguments"),
            ],
        ),
    };
    if let Some(rest) = path.strip_prefix("tool_calls.") {
        let (call, field) = rest.split_once('.')?;
        let call = parse_index(call)?;
        let name = call_fields
            .iter()
            .find_map(|(wire, name)| (*wire == field).then_some(*name))?;
        return Some(MessageField::ToolCall(call, name));
    }
    let name = match (layout, path) {
        (_, "role") => "role",
        (_, "content") => "content",
        (_, "tool_call_id") => "tool_call_id",
        (Layout::OpenLlmetry, "finish_reason") => "finish_reason",
        _ => return None,
    };
    Some(MessageField::Message(name))
}

/// A canonical decimal index: digits only, no leading zero except `0` itself,
/// so `01` and `1` cannot both claim one slot.
fn parse_index(text: &str) -> Option<u64> {
    let canonical = !text.is_empty()
        && text.bytes().all(|byte| byte.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'));
    canonical.then(|| text.parse().ok()).flatten()
}

/// Native content events named `name`, as `[{role?, content}]` in event
/// order, or `None` when the span has none.
fn event_messages(span: &Span, name: &str) -> Option<Value> {
    let messages: Vec<Value> = span
        .events
        .iter()
        .filter(|event| event.name == name)
        .filter_map(|event| {
            let mut message = Map::new();
            for field in ["role", "content"] {
                if let Some(value) = event.attributes.get(field).filter(|value| !value.is_null()) {
                    message.insert(field.to_owned(), field_value(value));
                }
            }
            (!message.is_empty()).then_some(Value::Object(message))
        })
        .collect();
    (!messages.is_empty()).then_some(Value::Array(messages))
}

/// The digest [`step_facts`] documents: a `$payload` reference's content
/// address, else the SHA-256 of the canonical JSON.
fn value_digest(value: &Value) -> String {
    payload_address(value).unwrap_or_else(|| {
        crate::payload::sha256_hex(crate::evals::canonical_json(value).as_bytes())
    })
}

/// The lowercase hex content address a well-formed `$payload` reference names.
/// Anything else — including a reference whose address is not 64 hex
/// characters — is an ordinary object and digests as one.
fn payload_address(value: &Value) -> Option<String> {
    let reference = value
        .as_object()?
        .get(crate::payload::PAYLOAD_KEY)?
        .as_str()?;
    let hex = reference.strip_prefix("sha256/")?;
    (hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| hex.to_ascii_lowercase())
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
    fn current_gen_ai_conventions_are_recognized() {
        // Current OTel GenAI names: gen_ai.provider.name, input/output tokens,
        // gen_ai.operation.name.
        let f = facts(&attrs(json!({
            "gen_ai.provider.name": "openai",
            "gen_ai.operation.name": "chat",
            "gen_ai.request.model": "gpt-4o",
            "gen_ai.usage.input_tokens": 120,
            "gen_ai.usage.output_tokens": 80,
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
    fn deprecated_gen_ai_names_are_accepted_as_aliases() {
        // gen_ai.system and prompt/completion tokens are OTel-deprecated but
        // still emitted by older instrumentation.
        let f = facts(&attrs(json!({
            "gen_ai.system": "anthropic",
            "gen_ai.request.model": "claude-sonnet",
            "gen_ai.usage.prompt_tokens": 40,
            "gen_ai.usage.completion_tokens": 25,
        })));
        assert!(f.is_llm);
        assert_eq!(f.provider.as_deref(), Some("anthropic"));
        assert_eq!(f.prompt_tokens, Some(40));
        assert_eq!(f.completion_tokens, Some(25));
        assert_eq!(f.total(), 65);
    }

    #[test]
    fn current_names_win_over_deprecated_when_both_present() {
        let f = facts(&attrs(json!({
            "gen_ai.provider.name": "openai",
            "gen_ai.system": "legacy",
            "gen_ai.request.model": "req-model",
            "gen_ai.response.model": "resp-model",
            "gen_ai.usage.input_tokens": 7,
            "gen_ai.usage.prompt_tokens": 999,
            "gen_ai.usage.output_tokens": 3,
            "gen_ai.usage.completion_tokens": 999,
        })));
        assert_eq!(
            f.provider.as_deref(),
            Some("openai"),
            "provider.name preferred"
        );
        assert_eq!(
            f.model.as_deref(),
            Some("resp-model"),
            "response model preferred"
        );
        assert_eq!(f.prompt_tokens, Some(7), "input_tokens preferred");
        assert_eq!(f.completion_tokens, Some(3), "output_tokens preferred");
        assert_eq!(f.total(), 10);
    }

    #[test]
    fn numeric_strings_coerce() {
        let f = facts(&attrs(json!({
            "gen_ai.request.model": "m",
            "gen_ai.usage.input_tokens": "120",
            "gen_ai.usage.output_tokens": "30",
            "llm.cost_usd": "0.25",
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
    fn provider_operation_or_span_kind_alone_marks_an_llm_span() {
        // The reviewer's runtime case: only gen_ai.provider.name +
        // gen_ai.operation.name present must still classify as an LLM span.
        assert!(
            facts(&attrs(
                json!({ "gen_ai.provider.name": "openai", "gen_ai.operation.name": "chat" })
            ))
            .is_llm
        );
        assert!(facts(&attrs(json!({ "gen_ai.operation.name": "embeddings" }))).is_llm);
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

    // ------------------------------------------------- OpenInference in facts

    #[test]
    fn openinference_keys_are_recognized_by_facts() {
        let f = facts(&attrs(json!({
            "openinference.span.kind": "LLM",
            "llm.model_name": "gpt-4o-mini",
            "llm.provider": "openai",
            "llm.system": "openai-system",
            "llm.token_count.prompt": 30,
            "llm.token_count.completion": "12",
            "llm.token_count.total": 42,
        })));
        assert!(f.is_llm);
        assert_eq!(f.model.as_deref(), Some("gpt-4o-mini"));
        assert_eq!(f.provider.as_deref(), Some("openai"), "llm.provider first");
        assert_eq!(f.prompt_tokens, Some(30));
        assert_eq!(f.completion_tokens, Some(12), "numeric strings coerce");
        assert_eq!(f.total_tokens, Some(42));

        let system_only = facts(&attrs(json!({ "llm.system": "anthropic" })));
        assert_eq!(system_only.provider.as_deref(), Some("anthropic"));
        assert!(system_only.is_llm, "a provider alone marks an LLM call");
    }

    #[test]
    fn every_previously_recognized_key_outranks_its_openinference_alias() {
        // Additivity: OpenInference ranks last, so a span that resolved a
        // value before the vocabulary existed resolves the same value now.
        let f = facts(&attrs(json!({
            "llm.model": "native-model",
            "llm.model_name": "oi-model",
            "gen_ai.system": "legacy-provider",
            "llm.provider": "oi-provider",
            "llm.system": "oi-system",
            "llm.prompt_tokens": 1,
            "llm.token_count.prompt": 100,
            "llm.completion_tokens": 2,
            "llm.token_count.completion": 200,
            "llm.total_tokens": 3,
            "llm.token_count.total": 300,
        })));
        assert_eq!(f.model.as_deref(), Some("native-model"));
        assert_eq!(f.provider.as_deref(), Some("legacy-provider"));
        assert_eq!(f.prompt_tokens, Some(1));
        assert_eq!(f.completion_tokens, Some(2));
        assert_eq!(f.total_tokens, Some(3));
        // And the provider pair ranks llm.provider over llm.system.
        let pair = facts(&attrs(
            json!({ "llm.provider": "azure", "llm.system": "openai" }),
        ));
        assert_eq!(pair.provider.as_deref(), Some("azure"));
    }

    #[test]
    fn an_openinference_llm_kind_alone_marks_an_llm_call() {
        for spelling in ["LLM", "llm", "Llm"] {
            assert!(
                facts(&attrs(json!({ "openinference.span.kind": spelling }))).is_llm,
                "{spelling}"
            );
        }
        for other in ["TOOL", "CHAIN", "RETRIEVER", "AGENT"] {
            assert!(
                !facts(&attrs(json!({ "openinference.span.kind": other }))).is_llm,
                "{other} is not an LLM call"
            );
        }
    }

    #[test]
    fn openinference_hosts_leave_cache_arithmetic_undecided() {
        // `aws` is OpenInference's host spelling, not `aws.bedrock`: with
        // cache counters present the context must stay "cannot tell".
        let f = facts(&attrs(json!({
            "llm.provider": "aws",
            "llm.token_count.prompt": 100,
            "gen_ai.usage.cache_read.input_tokens": 50,
        })));
        assert_eq!(f.context_tokens(), None);
        assert!(f.context_is_ambiguous());
    }

    // ------------------------------------------------------------ run identity

    #[test]
    fn run_id_follows_session_precedence_then_falls_back_to_the_trace() {
        for key in SESSION_KEYS {
            let attributes = attrs(json!({ key: "sess-7" }));
            assert_eq!(run_id(&attributes, "t1"), "sess-7", "{key} alone");
        }
        // Each key outranks every key after it.
        for (rank, winner) in SESSION_KEYS.iter().enumerate() {
            let mut attributes = Map::new();
            for (other_rank, key) in SESSION_KEYS.iter().enumerate().skip(rank) {
                attributes.insert((*key).to_owned(), json!(format!("from-{other_rank}")));
            }
            assert_eq!(
                run_id(&attributes, "t1"),
                format!("from-{rank}"),
                "{winner} must win over every later key"
            );
        }
        assert_eq!(
            run_id(&attrs(json!({ "gen_ai.conversation.id": 4711 })), "t1"),
            "4711",
            "a numeric session id is stringified, as session resolution does"
        );
        assert_eq!(
            run_id(
                &attrs(json!({ "session.id": "", "http.method": "GET" })),
                "t1"
            ),
            "trace:t1",
            "an empty session id is no session"
        );
        assert_eq!(run_id(&Map::new(), "abc123"), "trace:abc123");
    }

    #[test]
    fn run_trace_id_reads_only_the_trace_form() {
        assert_eq!(run_trace_id("trace:abc"), Some("abc"));
        assert_eq!(run_trace_id("trace:"), Some(""));
        assert_eq!(run_trace_id("sess-1"), None);
        assert_eq!(run_trace_id("Trace:abc"), None, "the prefix is exact");
        let attributes = attrs(json!({}));
        assert_eq!(
            run_trace_id(&run_id(&attributes, "t9")),
            Some("t9"),
            "run_id and run_trace_id round-trip"
        );
    }

    // -------------------------------------------------------------- step kind

    fn span_named(name: &str, attributes: Value) -> Span {
        Span {
            trace_id: "t".into(),
            span_id: "s".into(),
            tenant: String::new(),
            parent_span_id: None,
            name: name.into(),
            start_time_ns: 1,
            end_time_ns: 2,
            status: "ok".into(),
            service: "svc".into(),
            attributes: attrs(attributes),
            events: Vec::new(),
            links: Vec::new(),
            extra: Map::new(),
        }
    }

    fn step(attributes: Value) -> StepFacts {
        step_facts(&span_named("op", attributes))
    }

    fn kind_of(attributes: Value) -> StepKind {
        step(attributes).kind
    }

    fn sha(text: &str) -> String {
        crate::payload::sha256_hex(text.as_bytes())
    }

    #[test]
    fn every_operation_name_maps_to_its_kind() {
        for (operation, kind) in [
            ("chat", StepKind::Llm),
            ("text_completion", StepKind::Llm),
            ("generate_content", StepKind::Llm),
            ("execute_tool", StepKind::Tool),
            ("invoke_agent", StepKind::Agent),
            ("create_agent", StepKind::Agent),
            ("embeddings", StepKind::Embedding),
        ] {
            assert_eq!(
                kind_of(json!({ "gen_ai.operation.name": operation })),
                kind,
                "{operation}"
            );
        }
    }

    #[test]
    fn every_openinference_span_kind_maps_to_its_kind() {
        for (value, kind) in [
            ("LLM", StepKind::Llm),
            ("TOOL", StepKind::Tool),
            ("AGENT", StepKind::Agent),
            ("RETRIEVER", StepKind::Retrieval),
            ("EMBEDDING", StepKind::Embedding),
            ("CHAIN", StepKind::Other),
        ] {
            assert_eq!(
                kind_of(json!({ "openinference.span.kind": value })),
                kind,
                "{value}"
            );
        }
    }

    #[test]
    fn every_traceloop_span_kind_maps_to_its_kind() {
        for (value, kind) in [
            ("llm", StepKind::Llm),
            ("tool", StepKind::Tool),
            ("agent", StepKind::Agent),
            ("workflow", StepKind::Other),
            ("task", StepKind::Other),
        ] {
            assert_eq!(
                kind_of(json!({ "traceloop.span.kind": value })),
                kind,
                "{value}"
            );
        }
    }

    #[test]
    fn kind_falls_back_to_llm_facts_then_tool_names_then_other() {
        assert_eq!(kind_of(json!({ "llm.model": "m" })), StepKind::Llm);
        assert_eq!(kind_of(json!({ "llm.model_name": "m" })), StepKind::Llm);
        assert_eq!(
            kind_of(json!({ "llm.token_count.total": 9 })),
            StepKind::Llm
        );
        assert_eq!(
            kind_of(json!({ "gen_ai.tool.name": "search" })),
            StepKind::Tool
        );
        assert_eq!(kind_of(json!({ "tool.name": "search" })), StepKind::Tool);
        assert_eq!(kind_of(json!({ "http.method": "GET" })), StepKind::Other);
        assert_eq!(kind_of(json!({})), StepKind::Other);
        // traceloop.entity.name is a tool name only under a tool kind, so it
        // does not make a span a tool on its own.
        assert_eq!(
            kind_of(json!({ "traceloop.entity.name": "search" })),
            StepKind::Other
        );
    }

    #[test]
    fn kind_sources_rank_in_documented_order() {
        // operation name > OpenInference kind
        assert_eq!(
            kind_of(json!({ "gen_ai.operation.name": "execute_tool",
                            "openinference.span.kind": "LLM" })),
            StepKind::Tool
        );
        // OpenInference kind > Traceloop kind
        assert_eq!(
            kind_of(json!({ "openinference.span.kind": "AGENT",
                            "traceloop.span.kind": "tool" })),
            StepKind::Agent
        );
        // Traceloop kind > LLM facts: a workflow wrapper that records the
        // model it orchestrates is still a wrapper.
        assert_eq!(
            kind_of(json!({ "traceloop.span.kind": "workflow", "llm.model": "m" })),
            StepKind::Other
        );
        // CHAIN is a decision, not a fall-through.
        assert_eq!(
            kind_of(json!({ "openinference.span.kind": "CHAIN", "llm.model_name": "m" })),
            StepKind::Other
        );
        // LLM facts > tool-name presence.
        assert_eq!(
            kind_of(json!({ "llm.model": "m", "gen_ai.tool.name": "search" })),
            StepKind::Llm
        );
        // An unrecognized value falls through to the next source.
        assert_eq!(
            kind_of(json!({ "gen_ai.operation.name": "frobnicate",
                            "openinference.span.kind": "TOOL" })),
            StepKind::Tool
        );
        assert_eq!(
            kind_of(json!({ "openinference.span.kind": "GUARDRAIL",
                            "traceloop.span.kind": "agent" })),
            StepKind::Agent
        );
        assert_eq!(
            kind_of(json!({ "traceloop.span.kind": "unknown", "tool.name": "x" })),
            StepKind::Tool
        );
        // ...and an unknown operation name with nothing after it lands at llm,
        // because facts() counts any operation name as an LLM call.
        assert_eq!(
            kind_of(json!({ "gen_ai.operation.name": "frobnicate" })),
            StepKind::Llm
        );
    }

    #[test]
    fn kind_values_compare_case_insensitively() {
        assert_eq!(
            kind_of(json!({ "gen_ai.operation.name": "Chat" })),
            StepKind::Llm
        );
        assert_eq!(
            kind_of(json!({ "openinference.span.kind": "retriever" })),
            StepKind::Retrieval
        );
        assert_eq!(
            kind_of(json!({ "traceloop.span.kind": "TOOL" })),
            StepKind::Tool
        );
    }

    #[test]
    fn step_kinds_have_lowercase_wire_names_and_only_three_are_steps() {
        for (kind, wire, is_step) in [
            (StepKind::Llm, "llm", true),
            (StepKind::Tool, "tool", true),
            (StepKind::Agent, "agent", true),
            (StepKind::Retrieval, "retrieval", false),
            (StepKind::Embedding, "embedding", false),
            (StepKind::Other, "other", false),
        ] {
            assert_eq!(kind.as_str(), wire);
            assert_eq!(kind.to_string(), wire);
            assert_eq!(serde_json::to_value(kind).expect("serializes"), json!(wire));
            let back: StepKind = serde_json::from_value(json!(wire)).expect("deserializes");
            assert_eq!(back, kind);
            assert_eq!(kind.is_step(), is_step, "{wire}");
        }
        assert_eq!(StepKind::default(), StepKind::Other);
    }

    // ------------------------------------------------------ names and call ids

    #[test]
    fn tool_name_sources_rank_in_documented_order() {
        let all = json!({
            "traceloop.span.kind": "tool",
            "gen_ai.tool.name": "gen-ai-tool",
            "tool.name": "oi-tool",
            "traceloop.entity.name": "entity-tool",
        });
        assert_eq!(step(all).tool.as_deref(), Some("gen-ai-tool"));
        let without_gen_ai = json!({
            "traceloop.span.kind": "tool",
            "tool.name": "oi-tool",
            "traceloop.entity.name": "entity-tool",
        });
        assert_eq!(step(without_gen_ai).tool.as_deref(), Some("oi-tool"));
        let entity_only = json!({
            "traceloop.span.kind": "tool",
            "traceloop.entity.name": "entity-tool",
        });
        assert_eq!(step(entity_only).tool.as_deref(), Some("entity-tool"));
        // The entity name is a TOOL name only under a tool kind.
        for kind in ["llm", "task", "agent", "workflow"] {
            let facts = step(json!({
                "traceloop.span.kind": kind,
                "traceloop.entity.name": "entity",
            }));
            assert_eq!(facts.tool, None, "traceloop kind {kind}");
        }
    }

    #[test]
    fn agent_name_sources_rank_in_documented_order() {
        let all = json!({
            "traceloop.span.kind": "agent",
            "gen_ai.agent.name": "gen-ai-agent",
            "traceloop.entity.name": "entity-agent",
            "agent.name": "oi-agent",
        });
        assert_eq!(step(all).agent.as_deref(), Some("gen-ai-agent"));
        for kind in ["agent", "workflow"] {
            let facts = step(json!({
                "traceloop.span.kind": kind,
                "traceloop.entity.name": "entity-agent",
                "agent.name": "oi-agent",
            }));
            assert_eq!(facts.agent.as_deref(), Some("entity-agent"), "{kind}");
        }
        let oi_only = json!({ "agent.name": "oi-agent" });
        assert_eq!(step(oi_only).agent.as_deref(), Some("oi-agent"));
        // Under a tool kind the entity name is the tool's, not an agent's.
        let tool = step(json!({
            "traceloop.span.kind": "tool",
            "traceloop.entity.name": "search",
        }));
        assert_eq!(tool.agent, None);
        assert_eq!(tool.tool.as_deref(), Some("search"));
    }

    #[test]
    fn tool_call_id_reads_the_gen_ai_key() {
        assert_eq!(
            step(json!({ "gen_ai.tool.call.id": "call_1" }))
                .tool_call_id
                .as_deref(),
            Some("call_1")
        );
        assert_eq!(
            step(json!({ "gen_ai.tool.call.id": 7 }))
                .tool_call_id
                .as_deref(),
            Some("7"),
            "numeric ids stringify like every other identity"
        );
        assert_eq!(
            step(json!({ "gen_ai.tool.call.id": "" })).tool_call_id,
            None
        );
    }

    // ---------------------------------------------------- request and response

    /// Asserts a fallback chain: with every source present the first wins;
    /// removing the winner hands the answer to the next, down to the last.
    fn assert_chain(
        base: Value,
        sources: &[(&str, Value, Value)],
        read: impl Fn(&StepFacts) -> Option<Value>,
    ) {
        for start in 0..sources.len() {
            let mut attributes = attrs(base.clone());
            for (key, value, _) in &sources[start..] {
                attributes.insert((*key).to_owned(), value.clone());
            }
            let facts = step(Value::Object(attributes));
            assert_eq!(
                read(&facts),
                Some(sources[start].2.clone()),
                "with {} the highest-ranked source present",
                sources[start].0
            );
        }
    }

    #[test]
    fn llm_request_sources_rank_in_documented_order() {
        assert_chain(
            json!({ "gen_ai.operation.name": "chat" }),
            &[
                (
                    "gen_ai.input.messages",
                    json!(r#"[{"role":"user","parts":[{"type":"text","content":"a"}]}]"#),
                    json!([{"role": "user", "parts": [{"type": "text", "content": "a"}]}]),
                ),
                (
                    "gen_ai.prompt.0.content",
                    json!("b"),
                    json!([{"content": "b"}]),
                ),
                (
                    "llm.input_messages.0.message.content",
                    json!("c"),
                    json!([{"content": "c"}]),
                ),
                ("input.value", json!("d"), json!("d")),
                ("llm.prompt", json!("e"), json!("e")),
            ],
            |facts| facts.request.clone(),
        );
    }

    #[test]
    fn llm_response_sources_rank_in_documented_order() {
        assert_chain(
            json!({ "gen_ai.operation.name": "chat" }),
            &[
                (
                    "gen_ai.output.messages",
                    json!([{"role": "assistant", "parts": []}]),
                    json!([{"role": "assistant", "parts": []}]),
                ),
                (
                    "gen_ai.completion.0.finish_reason",
                    json!("stop"),
                    json!([{"finish_reason": "stop"}]),
                ),
                (
                    "llm.output_messages.0.message.content",
                    json!("c"),
                    json!([{"content": "c"}]),
                ),
                (
                    "output.value",
                    json!(r#"{"answer": 4}"#),
                    json!({"answer": 4}),
                ),
                ("llm.completion", json!("e"), json!("e")),
            ],
            |facts| facts.response.clone(),
        );
    }

    #[test]
    fn tool_request_and_response_sources_rank_in_documented_order() {
        let base = json!({ "gen_ai.operation.name": "execute_tool" });
        assert_chain(
            base.clone(),
            &[
                (
                    "gen_ai.tool.call.arguments",
                    json!(r#"{"q":"a"}"#),
                    json!({"q": "a"}),
                ),
                ("traceloop.entity.input", json!("b"), json!("b")),
                ("input.value", json!("c"), json!("c")),
            ],
            |facts| facts.request.clone(),
        );
        assert_chain(
            base,
            &[
                ("gen_ai.tool.call.result", json!(["a"]), json!(["a"])),
                ("traceloop.entity.output", json!("b"), json!("b")),
                ("output.value", json!("c"), json!("c")),
            ],
            |facts| facts.response.clone(),
        );
    }

    #[test]
    fn non_llm_non_tool_kinds_read_entity_input_and_output() {
        let base = json!({ "traceloop.span.kind": "agent" });
        assert_chain(
            base.clone(),
            &[
                ("traceloop.entity.input", json!("goal"), json!("goal")),
                ("input.value", json!("other"), json!("other")),
            ],
            |facts| facts.request.clone(),
        );
        assert_chain(
            base,
            &[
                ("traceloop.entity.output", json!("done"), json!("done")),
                ("output.value", json!("other"), json!("other")),
            ],
            |facts| facts.response.clone(),
        );
        // Tool-call keys belong to tool steps only.
        let agent = step(json!({
            "traceloop.span.kind": "agent",
            "gen_ai.tool.call.arguments": "{}",
        }));
        assert_eq!(agent.request, None);
    }

    #[test]
    fn llm_message_keys_are_not_read_for_a_tool_step() {
        let tool = step(json!({
            "gen_ai.operation.name": "execute_tool",
            "gen_ai.input.messages": "[]",
            "gen_ai.prompt.0.content": "x",
        }));
        assert_eq!(tool.request, None);
    }

    #[test]
    fn indexed_openllmetry_messages_reassemble_in_numeric_order() {
        let facts = step(json!({
            "gen_ai.operation.name": "chat",
            "gen_ai.prompt.0.role": "system",
            "gen_ai.prompt.0.content": "be brief",
            "gen_ai.prompt.1.role": "assistant",
            "gen_ai.prompt.1.content": "",
            "gen_ai.prompt.1.tool_calls.1.name": "fetch",
            "gen_ai.prompt.1.tool_calls.0.id": "call_a",
            "gen_ai.prompt.1.tool_calls.0.name": "search",
            "gen_ai.prompt.1.tool_calls.0.arguments": "{\"q\": \"traza\"}",
            "gen_ai.prompt.2.role": "tool",
            "gen_ai.prompt.2.tool_call_id": "call_a",
            "gen_ai.prompt.2.content": "[{\"type\":\"text\",\"text\":\"found\"}]",
            "gen_ai.prompt.10.role": "user",
            "gen_ai.prompt.10.content": "thanks",
            // Ignored: a non-canonical index, a non-numeric one, an
            // unreassembled field, and a null.
            "gen_ai.prompt.05.content": "leading zero",
            "gen_ai.prompt.x.content": "not an index",
            "gen_ai.prompt.3.user": "unknown field",
            "gen_ai.prompt.4.content": null,
        }));
        assert_eq!(
            facts.request,
            Some(json!([
                {"role": "system", "content": "be brief"},
                {"role": "assistant", "content": "", "tool_calls": [
                    {"id": "call_a", "name": "search", "arguments": {"q": "traza"}},
                    {"name": "fetch"},
                ]},
                {"role": "tool", "tool_call_id": "call_a",
                 "content": [{"type": "text", "text": "found"}]},
                {"role": "user", "content": "thanks"},
            ])),
            "10 sorts after 2; message content is kept verbatim (empty included) \
             and JSON-structured text is parsed"
        );
    }

    #[test]
    fn openinference_messages_reassemble_to_the_openllmetry_shape() {
        let openllmetry = step(json!({
            "gen_ai.operation.name": "chat",
            "gen_ai.completion.0.role": "assistant",
            "gen_ai.completion.0.content": "calling",
            "gen_ai.completion.0.tool_calls.0.id": "call_b",
            "gen_ai.completion.0.tool_calls.0.name": "search",
            "gen_ai.completion.0.tool_calls.0.arguments": "{\"q\":\"x\"}",
        }));
        let openinference = step(json!({
            "openinference.span.kind": "LLM",
            "llm.output_messages.0.message.role": "assistant",
            "llm.output_messages.0.message.content": "calling",
            "llm.output_messages.0.message.tool_calls.0.tool_call.id": "call_b",
            "llm.output_messages.0.message.tool_calls.0.tool_call.function.name": "search",
            "llm.output_messages.0.message.tool_calls.0.tool_call.function.arguments":
                "{\"q\":\"x\"}",
            // Not a message field: OpenInference nests everything under
            // `message.`, and it has no finish_reason.
            "llm.output_messages.0.role": "ignored",
            "llm.output_messages.0.message.finish_reason": "ignored",
        }));
        assert_eq!(openllmetry.response, openinference.response);
        assert_eq!(openllmetry.response_digest, openinference.response_digest);
        assert_eq!(
            openinference.response,
            Some(
                json!([{"role": "assistant", "content": "calling", "tool_calls": [
                    {"id": "call_b", "name": "search", "arguments": {"q": "x"}}
                ]}])
            )
        );
        let tool_message = step(json!({
            "openinference.span.kind": "LLM",
            "llm.input_messages.0.message.role": "tool",
            "llm.input_messages.0.message.tool_call_id": "call_b",
        }));
        assert_eq!(
            tool_message.request,
            Some(json!([{"role": "tool", "tool_call_id": "call_b"}]))
        );
    }

    #[test]
    fn native_events_are_the_last_llm_fallback() {
        let mut span = span_named("llm.completion", json!({ "llm.model": "m" }));
        span.events = vec![
            crate::Event {
                name: "llm.prompt".into(),
                timestamp_ns: 1,
                attributes: attrs(json!({ "role": "user", "content": "What is Traza?" })),
            },
            crate::Event {
                name: "unrelated".into(),
                timestamp_ns: 2,
                attributes: attrs(json!({ "content": "ignored" })),
            },
            crate::Event {
                name: "llm.completion".into(),
                timestamp_ns: 3,
                attributes: attrs(json!({ "content": "A tracing datastore." })),
            },
            crate::Event {
                name: "llm.prompt".into(),
                timestamp_ns: 4,
                attributes: attrs(json!({ "content": "{\"follow_up\": true}" })),
            },
            crate::Event {
                name: "llm.prompt".into(),
                timestamp_ns: 5,
                attributes: Map::new(),
            },
        ];
        let facts = step_facts(&span);
        assert_eq!(
            facts.request,
            Some(json!([
                {"role": "user", "content": "What is Traza?"},
                {"content": {"follow_up": true}},
            ])),
            "events in order; an event with neither role nor content is skipped"
        );
        assert_eq!(
            facts.response,
            Some(json!([{"content": "A tracing datastore."}]))
        );

        // An attribute of the same name outranks the events.
        span.attributes
            .insert("llm.prompt".into(), json!("attribute prompt"));
        assert_eq!(step_facts(&span).request, Some(json!("attribute prompt")));
    }

    #[test]
    fn system_instructions_lead_the_request() {
        let parts = step(json!({
            "gen_ai.operation.name": "chat",
            "gen_ai.system_instructions": "[{\"type\":\"text\",\"content\":\"be kind\"}]",
            "gen_ai.input.messages": [{"role": "user", "parts": []}],
        }));
        assert_eq!(
            parts.request,
            Some(json!([
                {"role": "system", "parts": [{"type": "text", "content": "be kind"}]},
                {"role": "user", "parts": []},
            ]))
        );
        let text = step(json!({
            "gen_ai.operation.name": "chat",
            "gen_ai.system_instructions": "be kind",
            "gen_ai.prompt.0.content": "hi",
        }));
        assert_eq!(
            text.request,
            Some(json!([{"role": "system", "content": "be kind"}, {"content": "hi"}]))
        );
        let alone = step(json!({
            "gen_ai.operation.name": "chat",
            "gen_ai.system_instructions": "be kind",
        }));
        assert_eq!(
            alone.request,
            Some(json!([{"role": "system", "content": "be kind"}]))
        );
        // A request that is not a list is kept whole, after them.
        let scalar = step(json!({
            "gen_ai.operation.name": "chat",
            "gen_ai.system_instructions": "be kind",
            "input.value": "plain prompt",
        }));
        assert_eq!(
            scalar.request,
            Some(json!([{"role": "system", "content": "be kind"}, "plain prompt"]))
        );
        // System instructions belong to llm steps only.
        let tool = step(json!({
            "gen_ai.operation.name": "execute_tool",
            "gen_ai.system_instructions": "be kind",
        }));
        assert_eq!(tool.request, None);
    }

    #[test]
    fn system_instructions_survive_a_request_that_is_not_a_list() {
        // An offloaded message list is a reference until it is resolved;
        // the system instructions recorded beside it must not be lost.
        let reference = json!({"$payload": format!("sha256/{}", "a".repeat(64))});
        let offloaded = step(json!({
            "gen_ai.operation.name": "chat",
            "gen_ai.system_instructions": "be kind",
            "gen_ai.input.messages": reference.clone(),
        }));
        let request = offloaded.request.expect("request");
        assert!(
            request.to_string().contains("be kind"),
            "system instructions dropped: {request}"
        );
        assert!(request.to_string().contains(&"a".repeat(64)), "{request}");
        // So two such steps differing only in their system prompt differ.
        let other = step(json!({
            "gen_ai.operation.name": "chat",
            "gen_ai.system_instructions": "be terse",
            "gen_ai.input.messages": reference,
        }));
        assert_ne!(other.request_digest, offloaded.request_digest);
    }

    #[test]
    fn step_extras_record_carriers_sources_and_empty_results() {
        let extras = |attributes: Value| {
            let span = span_named("op", attributes);
            step_facts_detailed(&span, &facts(&span.attributes)).1
        };
        let reference = json!({"$payload": format!("sha256/{}", "b".repeat(64))});
        // Carriers are kept apart only while one of them is a reference.
        let deferred = extras(json!({"gen_ai.operation.name": "chat",
            "gen_ai.system_instructions": "be kind", "gen_ai.input.messages": reference}));
        assert_eq!(
            deferred.request_carriers,
            Some(RequestCarriers {
                messages: Some(reference.clone()),
                system: json!("be kind"),
            })
        );
        let inline = extras(json!({"gen_ai.operation.name": "chat",
            "gen_ai.system_instructions": "be kind", "gen_ai.prompt.0.content": "hi"}));
        assert_eq!(inline.request_carriers, None);
        // The response's source, by the precedence of the response column.
        for (attributes, source) in [
            (
                json!({"gen_ai.operation.name": "chat", "gen_ai.completion.0.content": "a"}),
                ResponseSource::Messages,
            ),
            (
                json!({"gen_ai.operation.name": "chat", "output.value": "{\"choices\":[]}"}),
                ResponseSource::Body,
            ),
            (
                json!({"gen_ai.operation.name": "chat", "output.value": "{\"a\":1}",
                       "output.mime_type": "text/plain"}),
                ResponseSource::Text,
            ),
            (
                json!({"gen_ai.operation.name": "chat", "llm.completion": "{\"a\":1}"}),
                ResponseSource::Text,
            ),
        ] {
            assert_eq!(
                extras(attributes.clone()).response_source,
                source,
                "{attributes}"
            );
        }
        // A tool result recorded empty is told from one never recorded.
        let empty = extras(json!({"gen_ai.operation.name": "execute_tool",
            "gen_ai.tool.name": "f", "gen_ai.tool.call.result": ""}));
        assert_eq!(empty.empty_response, Some(json!("")));
        let absent = extras(json!({"gen_ai.operation.name": "execute_tool",
            "gen_ai.tool.name": "f"}));
        assert_eq!(absent.empty_response, None);
    }

    #[test]
    fn only_json_structured_text_is_parsed() {
        for (stored, read) in [
            (json!("{\"a\": 1}"), json!({"a": 1})),
            (json!("  [1, 2]"), json!([1, 2])),
            (json!("42"), json!("42")),
            (json!("true"), json!("true")),
            (json!("null"), json!("null")),
            (json!("\"quoted\""), json!("\"quoted\"")),
            (json!("{not json"), json!("{not json")),
            (json!(17), json!(17)),
            (
                json!({"already": "structured"}),
                json!({"already": "structured"}),
            ),
        ] {
            let facts = step(json!({
                "gen_ai.operation.name": "execute_tool",
                "gen_ai.tool.call.arguments": stored.clone(),
            }));
            assert_eq!(facts.request, Some(read), "stored {stored}");
        }
        // Null and the empty string are not sources at all.
        for absent in [json!(null), json!("")] {
            let facts = step(json!({
                "gen_ai.operation.name": "execute_tool",
                "gen_ai.tool.call.arguments": absent,
                "input.value": "fallback",
            }));
            assert_eq!(facts.request, Some(json!("fallback")));
        }
    }

    // ------------------------------------------------ digests and fingerprints

    #[test]
    fn digests_are_sha256_of_canonical_json() {
        // Pinned byte for byte: fingerprints are compared across processes
        // and builds, so the canonical form and the hash are format.
        let canonical = "[{\"content\":\"hi\",\"role\":\"user\"}]";
        let pinned = "4e79873118cd9be7a1f0308b9cd772950c5410c74ca3fe1ba2626cba009a9237";
        assert_eq!(sha(canonical), pinned);
        for stored in [
            json!("[ {\"role\": \"user\",   \"content\": \"hi\"} ]"),
            json!([{"role": "user", "content": "hi"}]),
            json!([{"content": "hi", "role": "user"}]),
        ] {
            let facts = step(json!({
                "gen_ai.operation.name": "chat",
                "gen_ai.input.messages": stored.clone(),
            }));
            assert_eq!(facts.request_digest.as_deref(), Some(pinned), "{stored}");
        }
        let output = step(json!({
            "gen_ai.operation.name": "chat",
            "gen_ai.output.messages": "[{\"role\":\"user\",\"content\":\"hi\"}]",
        }));
        assert_eq!(output.response_digest.as_deref(), Some(pinned));
        assert_eq!(output.request_digest, None, "no request, no digest");

        // A bare string digests as its JSON encoding, quotes included.
        let text = step(json!({ "gen_ai.operation.name": "chat", "input.value": "hi" }));
        assert_eq!(text.request_digest, Some(sha("\"hi\"")));
    }

    #[test]
    fn a_payload_reference_is_kept_unresolved_and_digests_to_its_address() {
        let address = "AB".repeat(32);
        let reference = json!({
            "$payload": format!("sha256/{address}"),
            "bytes": 120_000,
            "preview": "[{\"role\":\"user\"",
        });
        let facts = step(json!({
            "gen_ai.operation.name": "chat",
            "gen_ai.input.messages": reference.clone(),
            "gen_ai.output.messages": reference.clone(),
        }));
        assert_eq!(
            facts.request,
            Some(reference.clone()),
            "never resolved here"
        );
        assert_eq!(
            facts.request_digest.as_deref(),
            Some("ab".repeat(32).as_str()),
            "the content address, lowercased"
        );
        assert_eq!(facts.response_digest, facts.request_digest);
        // Beside system instructions the reference is no longer the whole
        // request, so the digest covers both (see the test below).
        let instructed = step(json!({
            "gen_ai.operation.name": "chat",
            "gen_ai.input.messages": reference.clone(),
            "gen_ai.system_instructions": "be kind",
        }));
        assert_eq!(
            instructed.request,
            Some(json!([{"role": "system", "content": "be kind"}, reference.clone()]))
        );
        assert_ne!(instructed.request_digest, facts.request_digest);

        // A redacted marker names its address too.
        let erased = step(json!({
            "gen_ai.operation.name": "chat",
            "gen_ai.input.messages": {"$payload": format!("sha256/{}", "c".repeat(64)),
                                      "bytes": 9, "erased": true},
        }));
        assert_eq!(erased.request_digest, Some("c".repeat(64)));

        // A malformed reference is an ordinary object.
        let malformed = json!({ "$payload": "sha256/abc" });
        let facts = step(json!({
            "gen_ai.operation.name": "chat",
            "gen_ai.input.messages": malformed.clone(),
        }));
        assert_eq!(
            facts.request_digest,
            Some(sha(&crate::evals::canonical_json(&malformed)))
        );
    }

    #[test]
    fn fingerprints_are_kind_name_and_request_hash() {
        let tool = step(json!({
            "gen_ai.operation.name": "execute_tool",
            "gen_ai.tool.name": "web_search",
            "gen_ai.tool.call.arguments": "{\"q\": \"traza\"}",
        }));
        // sha256 of the canonical `{"q":"traza"}`, pinned.
        assert_eq!(tool.fingerprint, "tool:web_search#50951da9bae64397");
        assert_eq!(tool.signature(), "tool:web_search");

        let llm = step(json!({
            "gen_ai.operation.name": "chat",
            "gen_ai.request.model": "gpt-4o",
            "gen_ai.input.messages": [{"role": "user", "content": "hi"}],
        }));
        assert_eq!(llm.fingerprint, "llm:gpt-4o#4e79873118cd9be7");
        assert_eq!(llm.signature(), "llm:gpt-4o");

        let agent = step(json!({
            "gen_ai.operation.name": "invoke_agent",
            "gen_ai.agent.name": "planner",
        }));
        assert_eq!(agent.fingerprint, "agent:planner", "no request, no hash");
        assert_eq!(agent.signature(), "agent:planner");

        // Each kind falls back to the span name when its own name is absent.
        let unnamed = [
            json!({ "gen_ai.operation.name": "chat" }),
            json!({ "gen_ai.operation.name": "execute_tool" }),
            json!({ "gen_ai.operation.name": "invoke_agent" }),
            json!({ "openinference.span.kind": "RETRIEVER", "llm.model_name": "m" }),
            json!({ "http.method": "GET" }),
        ];
        for (attributes, expected) in unnamed.into_iter().zip([
            "llm:openai.chat",
            "tool:openai.chat",
            "agent:openai.chat",
            "retrieval:openai.chat",
            "other:openai.chat",
        ]) {
            let facts = step_facts(&span_named("openai.chat", attributes));
            assert_eq!(facts.fingerprint, expected);
        }

        // A name containing '#' does not confuse the signature.
        let hashed = step(json!({
            "gen_ai.tool.name": "a#b",
            "gen_ai.tool.call.arguments": "{\"q\": \"traza\"}",
        }));
        assert_eq!(hashed.fingerprint, "tool:a#b#50951da9bae64397");
        assert_eq!(hashed.signature(), "tool:a#b");
    }

    #[test]
    fn the_same_call_fingerprints_identically_and_a_new_input_does_not() {
        let call = |arguments: &str| {
            step(json!({
                "gen_ai.operation.name": "execute_tool",
                "gen_ai.tool.name": "search",
                "gen_ai.tool.call.arguments": arguments,
                // The output and the call id do not identify the call.
                "gen_ai.tool.call.result": arguments.len().to_string(),
                "gen_ai.tool.call.id": format!("call-{}", arguments.len()),
            }))
        };
        let first = call("{\"q\": \"traza\", \"page\": 1}");
        let repeat = call("{\"page\":1,\"q\":\"traza\"}");
        let different = call("{\"q\": \"traza\", \"page\": 2}");
        assert_eq!(first.fingerprint, repeat.fingerprint);
        assert_ne!(first.fingerprint, different.fingerprint);
        assert_eq!(first.signature(), different.signature());
    }

    #[test]
    fn step_facts_with_reuses_the_callers_facts() {
        // The store passes PRICED facts; nothing pricing touches changes a
        // step fact, so both entry points agree.
        let span = span_named(
            "chat",
            json!({ "gen_ai.request.model": "m", "gen_ai.usage.input_tokens": 3 }),
        );
        let mut priced = facts(&span.attributes);
        priced.cost_usd = Some(1.0);
        priced.cost_derived = true;
        assert_eq!(step_facts_with(&span, &priced), step_facts(&span));
    }

    #[test]
    fn the_fingerprint_alone_is_the_fingerprint_step_facts_report() {
        // The run guard keys repeats on `step_fingerprint_with`, and every
        // other surface shows `step_facts().fingerprint`: one shape per kind
        // and name source, with and without a request, must agree.
        let shapes = [
            json!({"gen_ai.operation.name": "chat", "gen_ai.request.model": "gpt-4o",
                   "gen_ai.input.messages": "[{\"role\":\"user\",\"content\":\"hi\"}]",
                   "gen_ai.output.messages": "[{\"role\":\"assistant\",\"content\":\"yo\"}]"}),
            json!({"gen_ai.operation.name": "chat"}),
            json!({"gen_ai.operation.name": "execute_tool", "gen_ai.tool.name": "search",
                   "gen_ai.tool.call.arguments": "{\"q\":1}",
                   "gen_ai.tool.call.result": "{\"hits\":3}"}),
            json!({"traceloop.span.kind": "tool", "traceloop.entity.name": "fetch",
                   "traceloop.entity.input": "{\"url\":\"x\"}"}),
            json!({"gen_ai.operation.name": "invoke_agent", "gen_ai.agent.name": "planner",
                   "input.value": "plan it"}),
            json!({"traceloop.span.kind": "workflow", "traceloop.entity.name": "flow",
                   "traceloop.entity.input": "go"}),
            json!({"openinference.span.kind": "TOOL", "tool.name": "calc"}),
            json!({"http.method": "GET"}),
        ];
        for attributes in shapes {
            let span = span_named("op", attributes);
            let llm = facts(&span.attributes);
            assert_eq!(
                step_fingerprint_with(&span, &llm),
                step_facts_with(&span, &llm).fingerprint,
                "{:?}",
                span.attributes
            );
        }
    }
}
