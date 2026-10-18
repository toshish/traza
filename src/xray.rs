//! Context X-Ray and Cache Autopsy: exactly what a model saw, where each
//! part of it came from, and why its prompt cache held or broke.
//!
//! Traza already stores every request an agent sent. This module reads one
//! model call's request as the context window it was — split into chunks,
//! one per message part ([`chunk`]) — and answers the questions a person
//! debugging an agent actually asks of it:
//!
//! - **Where did this come from?** Each chunk's *provenance*: the earliest
//!   step of the run whose request or response contained the same content.
//!   A tool result is introduced by the tool step that returned it (matched
//!   on content, else on its tool-call id when the agent changed the text
//!   before showing it to the model); the model's own earlier words by the
//!   call that said them; a system prompt by the first call that carried it.
//! - **Is it dead weight?** A tool result older than `--xray-stale-steps`
//!   steps that no model output since has drawn on is *stale*. "Drawn on" is
//!   a heuristic, stated as one: some distinctive word of the result (four to
//!   sixty-four letters, digits or `_./-`) first appears in a model output —
//!   an answer, or a tool call's arguments — after the result arrived. A
//!   result nobody quoted is not proven unused, and a quoted one not proven
//!   used, but a long-lived result the model never echoed is the first thing
//!   to cut when a context is too big. When no model output after the result
//!   was recorded at all, nothing is called stale: there is nothing to judge
//!   by.
//! - **Is anything in here twice?** Chunks with identical content.
//! - **What was lost?** The chunks the conversation's previous call had and
//!   this one does not — a compaction-loss detector. A lost system or user
//!   chunk is flagged an `instruction`: a constraint the agent was given
//!   and may no longer see. A chunk that came back edited — the same role
//!   and kind, differing in a stretch at most a tenth of its text — is
//!   reported as edited, with the bytes that changed, not as lost.
//! - **Why did the cache break?** The Cache Autopsy ([`autopsy`]) compares
//!   each call with the previous call of its conversation and names the first
//!   chunk that differs, the bytes that changed in it — usually one volatile
//!   token, a timestamp or a UUID — the cause, and what it cost.
//!
//! **Everything is deterministic arithmetic over stored spans.** No model is
//! called or embedded, nothing is learned, nothing leaves the process. Token
//! counts per chunk are estimates (about four ASCII characters, or one and a
//! half other characters, per token); the provider's own counts are reported
//! beside them, and the autopsy scales its estimates by the ratio between the
//! two before pricing anything.
//!
//! **Bounds.** One request reads its run under the store's snapshot, erasure
//! and tenant rules ([`crate::run`]), inside the query deadline, at most
//! [`MAX_TAPE_SPANS`] spans; resolves at most [`MAX_READ_BYTES`] of offloaded
//! payload content for the history and as much again for the call itself;
//! splits one context into at most [`MAX_CONTEXT_CHUNKS`] chunks; keeps at
//! most [`MAX_PROVENANCE_ENTRIES`] content digests for provenance,
//! [`MAX_SAID_WORDS`] words for staleness and [`MAX_POOL_BYTES`] of chunk
//! text for diffs; and an autopsy analyses at most [`MAX_AUTOPSY_STEPS`]
//! model calls. Past any bound the answer says so (`chunks_truncated`,
//! `provenance_partial`, `resolved_partial`, `texts_partial`, `truncated`)
//! rather than looking complete.

mod autopsy;
mod chunk;
pub mod http;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::time::Instant;

use serde::Serialize;
use serde_json::Value;

pub use autopsy::{ByteDiff, Cause, Change, Divergence, Volatile};
pub use chunk::ChunkKind;

use crate::semconv::{self, LlmFacts, StepKind};
use crate::tape::{payload_reference, ResolveBudget, Resolver, TapeStep, MAX_TAPE_SPANS};
use crate::{Deadline, Result, Span, Store};
use autopsy::{Comparison, Context, TextPool};
use chunk::{Piece, TOOLS_ROLE};

/// Default steps after which an unreferenced tool result is stale
/// (`--xray-stale-steps`).
pub const DEFAULT_STALE_STEPS: usize = 20;

/// Default prompt-cache lifetime the autopsy assumes, in seconds: five
/// minutes, the default lifetime of an Anthropic cache entry and the low end
/// of OpenAI's. A deployment on longer-lived caches passes `ttl_seconds`.
pub const DEFAULT_CACHE_TTL_SECONDS: u64 = 300;

/// Chunks one context is split into at most; past it the rest is not
/// analysed and `chunks_truncated` says so.
pub const MAX_CONTEXT_CHUNKS: usize = 2_000;

/// Offloaded payload bytes one request resolves for the run's history (and,
/// separately, for the call under the X-Ray itself). Payload content is read
/// to be chunked and digested, never returned whole, so this bounds reading
/// work rather than response size; the query deadline bounds it too.
pub const MAX_READ_BYTES: usize = 64 * 1024 * 1024;

/// Distinct chunk text kept for diffs and previews in one request.
pub const MAX_POOL_BYTES: usize = 32 * 1024 * 1024;

/// Content digests kept for provenance in one request. Past it, only the
/// examined call's own digests are still tracked and `provenance_partial`
/// is set.
pub const MAX_PROVENANCE_ENTRIES: usize = 200_000;

/// Distinct words of model output tracked for staleness. Past it staleness
/// is not judged at all (`provenance_partial`), since a word missed would
/// make a referenced result look stale.
pub const MAX_SAID_WORDS: usize = 500_000;

/// Model calls one autopsy analyses at most.
pub const MAX_AUTOPSY_STEPS: usize = 2_000;

/// Dropped chunks one X-Ray lists at most.
const MAX_DROPPED: usize = 256;

/// Duplicate groups one X-Ray lists at most.
const MAX_DUPLICATES: usize = 64;

/// Text comparisons one X-Ray spends pairing dropped chunks with edited
/// ones, and the longest text it compares.
const MAX_EDIT_COMPARISONS: usize = 4_096;
const MAX_EDIT_BYTES: usize = 64 * 1024;

/// Sources the composition names before folding the rest.
const MAX_SOURCES: usize = 12;

/// Earlier calls searched for a call's predecessor.
const RECENT_CONTEXTS: usize = 32;

/// Characters of a chunk preview.
const PREVIEW_CHARS: usize = 120;

/// How a calibration ratio is clamped: an estimate off by more than four
/// times either way says the estimate is not about the same text.
const CALIBRATION_RANGE: (f64, f64) = (0.25, 4.0);

/// What an X-Ray should do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XrayOptions {
    /// Resolve offloaded `$payload` references before chunking. Without it an
    /// offloaded request is one opaque chunk.
    pub resolve: bool,
    /// Steps after which an unreferenced tool result is stale.
    pub stale_steps: usize,
    /// The cache lifetime the embedded cache verdict assumes, in seconds.
    pub ttl_seconds: u64,
}

impl Default for XrayOptions {
    fn default() -> Self {
        Self {
            resolve: true,
            stale_steps: DEFAULT_STALE_STEPS,
            ttl_seconds: DEFAULT_CACHE_TTL_SECONDS,
        }
    }
}

/// What a Cache Autopsy should do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AutopsyOptions {
    /// The cache lifetime to assume, in seconds.
    pub ttl_seconds: u64,
    /// Resolve offloaded requests before comparing them.
    pub resolve: bool,
}

impl Default for AutopsyOptions {
    fn default() -> Self {
        Self {
            ttl_seconds: DEFAULT_CACHE_TTL_SECONDS,
            resolve: true,
        }
    }
}

/// Why an X-Ray was not produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum XrayMiss {
    /// No span with that trace and span id in scope.
    NoSpan,
    /// The span is a step of another kind; the X-Ray reads model calls.
    NotAnLlmStep(StepKind),
    /// The operator view found the ids in this many tenants.
    AmbiguousTenant(usize),
}

/// A span's address.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SpanRef {
    /// The trace.
    pub trace_id: String,
    /// The span.
    pub span_id: String,
}

/// The step that introduced a chunk.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Introduced {
    /// Its trace.
    pub trace_id: String,
    /// Its span.
    pub span_id: String,
    /// Its tape index.
    pub step_index: usize,
    /// Its step kind.
    pub kind: StepKind,
    /// Its tool, model or agent name, else the span name.
    pub name: String,
    /// `content` when the step held the same content; `tool_call_id` when
    /// it is the tool step that answered this result's call but returned
    /// different text than the model was shown.
    pub via: &'static str,
}

/// One chunk of the context window.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Chunk {
    /// Position in the context, from zero.
    pub index: usize,
    /// The message role as recorded (lowercased); `tools` for a tool
    /// definition; `null` for a bare prompt.
    pub role: Option<String>,
    /// What the chunk is.
    pub kind: ChunkKind,
    /// Characters of its text.
    pub chars: usize,
    /// Estimated tokens.
    pub est_tokens: u64,
    /// 16 hex characters identifying its content.
    pub digest: String,
    /// The earliest step that held it; `null` when that could not be
    /// established.
    pub introduced_by: Option<Introduced>,
    /// Steps between its introduction and this call.
    pub age_steps: Option<usize>,
    /// A tool result older than the stale threshold that no model output
    /// since has drawn on (a heuristic; see the module docs).
    pub stale: bool,
    /// The first 120 characters of its text.
    pub preview: String,
    /// The tool a tool call or definition names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The tool call id a call or result carries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// The index of the first chunk with the same content, when this one
    /// repeats it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duplicate_of: Option<usize>,
}

/// Sizes of the context, estimated and reported.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Totals {
    /// Chunks analysed.
    pub chunks: usize,
    /// Their estimated tokens.
    pub est_tokens: u64,
    /// The provider's prompt count, as reported.
    pub reported_input_tokens: Option<u64>,
    /// The context the call carried, resolved across providers' cache
    /// conventions ([`LlmFacts::context_tokens`]); `null` when that cannot
    /// be said.
    pub reported_context_tokens: Option<u64>,
    /// Prompt tokens served from the provider's cache, as reported.
    pub cache_read: Option<u64>,
    /// Prompt tokens written into the provider's cache, as reported.
    pub cache_write: Option<u64>,
    /// Stale chunks.
    pub stale_chunks: usize,
    /// Their estimated tokens.
    pub stale_est_tokens: u64,
    /// Estimated tokens spent on repeats (every copy after the first).
    pub duplicate_est_tokens: u64,
}

/// One row of a composition.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Share {
    /// What the row counts: a chunk kind, or a source such as `tool:grep`.
    pub key: String,
    /// Chunks.
    pub chunks: usize,
    /// Estimated tokens.
    pub est_tokens: u64,
    /// Fraction of the context's estimated tokens, 0–1.
    pub share: f64,
}

/// What the context is made of.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Composition {
    /// By chunk kind, largest first.
    pub by_kind: Vec<Share>,
    /// By source — `system`, `user`, `assistant`, `tool calls`, `tool
    /// definitions`, `tool:<name>` for a tool's results, `other` — largest
    /// first, the smallest folded into `other sources` past twelve.
    pub by_source: Vec<Share>,
}

/// The call this one is compared with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PreviousRef {
    /// Its trace.
    pub trace_id: String,
    /// Its span.
    pub span_id: String,
    /// Its tape index.
    pub step_index: usize,
    /// Its chunks this call still holds.
    pub shared_chunks: usize,
}

/// A chunk the previous call had and this one does not.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Dropped {
    /// Its content digest.
    pub digest: String,
    /// Its role.
    pub role: Option<String>,
    /// `instruction` for a system or user chunk, else `other`.
    pub kind: &'static str,
    /// Its chunk kind.
    pub chunk_kind: ChunkKind,
    /// Estimated tokens.
    pub est_tokens: u64,
    /// The first 120 characters of its text, when kept.
    pub preview: Option<String>,
    /// The earliest step that held it.
    pub introduced_by: Option<Introduced>,
}

/// A chunk the previous call had, carried into this one with an edit.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Edited {
    /// Its position in this call's context.
    pub index: usize,
    /// Its position in the previous call's context.
    pub previous_index: usize,
    /// Its role.
    pub role: Option<String>,
    /// Its chunk kind.
    pub kind: ChunkKind,
    /// The first differing stretch, widened to the word it falls in.
    pub diff: ByteDiff,
    /// What kind of token was swapped.
    pub volatile: Volatile,
}

/// Chunks repeating the same content.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Duplicate {
    /// The shared content digest.
    pub digest: String,
    /// How many chunks carry it.
    pub count: usize,
    /// Estimated tokens of one copy.
    pub est_tokens_each: u64,
    /// The chunks' positions.
    pub indices: Vec<usize>,
    /// The first 120 characters of the text.
    pub preview: String,
}

/// One model call's cache verdict.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CacheStep {
    /// Its tape index.
    pub step_index: usize,
    /// Its span.
    pub span: SpanRef,
    /// The model it named.
    pub model: Option<String>,
    /// The predecessor it was compared with.
    pub previous_step_index: Option<usize>,
    /// Chunks in its context.
    pub chunks: usize,
    /// Their estimated tokens.
    pub est_tokens: u64,
    /// Leading chunks identical to the predecessor's.
    pub prefix_chunks_shared: usize,
    /// Their estimated tokens.
    pub shared_est_tokens: u64,
    /// Where the prefix diverged, when it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_divergent_chunk: Option<Divergence>,
    /// Cache-read tokens, as reported.
    pub cache_read: Option<u64>,
    /// Cache-write tokens, as reported.
    pub cache_write: Option<u64>,
    /// Whether the provider reported any cache read.
    pub reported_hit: Option<bool>,
    /// Time since the predecessor began, which is when its prompt entered
    /// the cache.
    pub gap_ns: Option<u64>,
    /// The verdict.
    pub cause: Cause,
    /// The verdict as a sentence. Quotes recorded text.
    pub explanation: String,
    /// Estimated tokens the cache could have served and did not, scaled to
    /// the provider's count when it reported one.
    pub est_tokens_lost: u64,
    /// What those tokens cost beyond a cache read, when the pricing table
    /// names the model's input and cache-read rates.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub est_cost_lost_usd: Option<f64>,
}

/// A recurring cause of cache breaks.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Culprit {
    /// The cause.
    pub cause: Cause,
    /// What happened at the divergent chunk.
    pub change: Option<Change>,
    /// The divergent chunk's role.
    pub role: Option<String>,
    /// The kind of token swapped, for edits.
    pub volatile: Option<Volatile>,
    /// Breaks of this shape.
    pub breaks: usize,
    /// Their estimated lost tokens.
    pub est_tokens_lost: u64,
    /// Their estimated cost, when priced.
    pub est_cost_lost_usd: Option<f64>,
    /// The first break of this shape.
    pub example_step_index: usize,
    /// Its diff, when there was one.
    pub example: Option<ByteDiff>,
    /// What usually removes a break of this shape: fixed guidance chosen by
    /// the shape, not generated.
    pub advice: &'static str,
}

/// The fixed guidance for a break shape.
fn advice(cause: Cause, change: Option<Change>, volatile: Option<Volatile>) -> &'static str {
    match (cause, change, volatile) {
        (Cause::VolatileContent, Some(Change::Edited), Some(Volatile::Timestamp)) => {
            "A clock value sits inside the cached prefix. Move the time to the end of the \
             prompt (after the last cache breakpoint), or round it to a coarser unit such as \
             the day."
        }
        (Cause::VolatileContent, Some(Change::Edited), Some(Volatile::Uuid | Volatile::HexId)) => {
            "A per-request id sits inside the cached prefix. Move it out of the prompt, or \
             after the last cache breakpoint."
        }
        (Cause::VolatileContent, Some(Change::Edited), Some(Volatile::Number)) => {
            "A changing number (a counter, a balance, a step count) sits inside the cached \
             prefix. Move it to the end of the prompt."
        }
        (Cause::VolatileContent, Some(Change::Edited), _) => {
            "A chunk inside the cached prefix is rewritten between calls. Keep stable content \
             first and put what changes per call at the end."
        }
        (Cause::VolatileContent, Some(Change::Inserted), _) => {
            "New content is inserted ahead of earlier turns (retrieved documents or a status \
             block placed before the history). Append it after the history instead."
        }
        (Cause::Reordered, Some(Change::Reserialized), _) => {
            "The same content is serialized into different bytes between calls (key order, \
             spacing, or a role change). Serialize deterministically, keys sorted, before it \
             enters the prompt."
        }
        (Cause::Reordered, _, _) => {
            "Chunks change order between calls (tool definitions or retrieved documents \
             re-ranked). Keep a stable order."
        }
        (Cause::NewContext, Some(Change::Removed), _) => {
            "Old turns are pruned from inside the prefix, and every pruning invalidates the \
             cache after it. Prune in larger, rarer batches."
        }
        (Cause::NewContext, _, _) => {
            "The context was rewritten (compaction or truncation). Expected when deliberate; \
             check the X-Ray's dropped_since_previous for instructions it lost."
        }
        (Cause::TtlExpired, _, _) => {
            "Calls are further apart than the cache lifetime. Use a longer-lived cache if the \
             provider offers one, or pass ttl_seconds= if it is already in use."
        }
        (Cause::Unknown, _, _) | (Cause::Stable, _, _) | (Cause::VolatileContent, _, _) => {
            "The trace does not say enough to explain this break; open the step's X-Ray."
        }
    }
}

/// An autopsy's totals.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CacheSummary {
    /// Model calls analysed.
    pub steps: usize,
    /// Calls with a predecessor whose verdict is not `stable`.
    pub breaks: usize,
    /// Calls per cause, every cause listed.
    pub by_cause: BTreeMap<&'static str, usize>,
    /// Estimated tokens lost across the run.
    pub est_tokens_lost: u64,
    /// Their cost, summed over the calls the table could price; `null` when
    /// it could price none.
    pub est_cost_lost_usd: Option<f64>,
    /// Breaks the table could not price (no cache-read rate for the model).
    pub unpriced_breaks: usize,
    /// Calls where the recorded structure and the provider's report
    /// disagree: a stable prefix with no cache read reported, or a read
    /// reported where nothing could have been served.
    pub disagreements: usize,
    /// Fixed guidance when there are disagreements; `null` otherwise.
    pub disagreement_advice: Option<&'static str>,
    /// Recurring break shapes, costliest first.
    pub culprits: Vec<Culprit>,
}

/// What usually explains a disagreement between the structure and the
/// provider's report.
const DISAGREEMENT_ADVICE: &str = "The provider's report contradicts the recorded prompts: a \
    prefix that did not change was not served from its cache, or a read was reported where \
    nothing could be served. Check that the calls mark a cache breakpoint (Anthropic's \
    cache_control), that the prefix exceeds the provider's minimum cacheable length, and that \
    ttl_seconds matches the cache lifetime in use.";

/// A run's Cache Autopsy.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CacheAutopsy {
    /// The run asked for.
    pub run: String,
    /// The tenant scope; `null` for the operator view.
    pub tenant: Option<String>,
    /// The cache lifetime assumed.
    pub ttl_seconds: u64,
    /// Every model call, in step order.
    pub steps: Vec<CacheStep>,
    /// Totals.
    pub summary: CacheSummary,
    /// The run had more spans or model calls than one autopsy reads.
    pub truncated: bool,
    /// Some offloaded request could not be resolved.
    pub resolved_partial: bool,
    /// Some chunk text was not kept for lack of room, so a diff is `null`.
    pub texts_partial: bool,
}

/// One model call's context window, X-rayed.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct XRay {
    /// The run the call belongs to.
    pub run: String,
    /// The call's tenant.
    pub tenant: String,
    /// The call.
    pub span: SpanRef,
    /// Its tape index; `null` when the run is longer than one read and the
    /// call lies past it.
    pub step_index: Option<usize>,
    /// The model it named.
    pub model: Option<String>,
    /// The provider it named.
    pub provider: Option<String>,
    /// When it began.
    pub start_ns: u64,
    /// The context window, in order.
    pub chunks: Vec<Chunk>,
    /// Why the context could not be read (`"was not recorded"`, `"was
    /// offloaded and could not be resolved"`, `"was offloaded and
    /// resolve=false"`), or `null` when it was.
    pub context_unavailable: Option<&'static str>,
    /// The context had more than [`MAX_CONTEXT_CHUNKS`] chunks.
    pub chunks_truncated: bool,
    /// Sizes.
    pub totals: Totals,
    /// What it is made of.
    pub composition: Composition,
    /// The conversation's previous call, when there is one.
    pub previous: Option<PreviousRef>,
    /// What that call had and this one lost.
    pub dropped_since_previous: Vec<Dropped>,
    /// More was dropped than listed.
    pub dropped_truncated: bool,
    /// Chunks the previous call had that this one carries edited: the same
    /// role and kind, differing in a stretch at most a tenth of the text —
    /// a changed instruction, not a lost one.
    pub edited_since_previous: Vec<Edited>,
    /// Repeated content.
    pub duplicates: Vec<Duplicate>,
    /// The call's cache verdict.
    pub cache: CacheStep,
    /// The stale threshold applied.
    pub stale_after_steps: usize,
    /// The model's output, chunked the same way.
    pub response: Vec<Chunk>,
    /// Some offloaded value could not be resolved (or `resolve=false`).
    pub resolved_partial: bool,
    /// A provenance or staleness bound was reached, or the history was cut
    /// short, so `introduced_by` and `stale` are incomplete.
    pub provenance_partial: bool,
}

/// A step paired with its span.
struct Paired<'t> {
    step: &'t TapeStep,
    span: &'t Span,
}

/// Pairs each step with its span: steps are a subsequence of the spans, in
/// the same order.
fn pair<'t>(steps: &'t [TapeStep], spans: &'t [Span]) -> Vec<Paired<'t>> {
    let mut paired = Vec::with_capacity(steps.len());
    let mut cursor = spans.iter();
    for step in steps {
        for span in cursor.by_ref() {
            if span.trace_id == step.trace_id
                && span.span_id == step.span_id
                && span.tenant == step.detail.tenant
            {
                paired.push(Paired { step, span });
                break;
            }
        }
    }
    paired
}

/// Whether a value holds a `$payload` reference anywhere.
fn has_reference(value: &Value) -> bool {
    if payload_reference(value).is_some() {
        return true;
    }
    match value {
        Value::Array(items) => items.iter().any(has_reference),
        Value::Object(fields) => fields.values().any(has_reference),
        _ => false,
    }
}

/// Why a context cannot be read, if it cannot.
fn unavailable(request: Option<&Value>, resolve: bool) -> Option<&'static str> {
    match request {
        None => Some("was not recorded"),
        Some(value) if has_reference(value) && resolve => {
            Some("was offloaded and could not be resolved")
        }
        Some(value) if has_reference(value) => Some("was offloaded and resolve=false"),
        Some(_) => None,
    }
}

/// A model call's context and output, read and chunked.
struct Read {
    context: chunk::Pieces,
    response: Vec<Piece>,
    unavailable: Option<&'static str>,
}

/// Resolves (when asked) and chunks one model call.
fn read_call(
    paired: &Paired<'_>,
    resolver: &mut Resolver<'_>,
    budget: &mut ResolveBudget,
    resolve: bool,
    with_response: bool,
) -> Result<Read> {
    let mut step = paired.step.clone();
    let mut definitions = chunk::tool_definitions(&paired.span.attributes);
    if resolve {
        resolver.resolve_request(&mut step, budget)?;
        for definition in &mut definitions {
            resolver.resolve(definition, budget)?;
        }
        if with_response {
            if let Some(response) = step.response.as_mut() {
                resolver.resolve(response, budget)?;
            }
        }
    }
    let context = chunk::context_pieces(step.request.as_ref(), &definitions, MAX_CONTEXT_CHUNKS);
    let response = match (&step.response, with_response) {
        (Some(response), true) => chunk::response_pieces(response, MAX_CONTEXT_CHUNKS).list,
        _ => Vec::new(),
    };
    Ok(Read {
        context,
        response,
        unavailable: unavailable(step.request.as_ref(), resolve),
    })
}

/// The name a step goes by: tool, model, agent, else span name.
fn step_name(step: &TapeStep) -> String {
    step.tool
        .as_deref()
        .or(step.model.as_deref())
        .or(step.agent.as_deref())
        .unwrap_or(step.name.as_str())
        .to_owned()
}

fn introduced(step: &TapeStep, via: &'static str) -> Introduced {
    Introduced {
        trace_id: step.trace_id.clone(),
        span_id: step.span_id.clone(),
        step_index: step.index,
        kind: step.kind,
        name: step_name(step),
        via,
    }
}

/// The ratio of the provider's context count to the estimate, clamped, or 1.
fn calibration(llm: &LlmFacts, est_tokens: u64) -> f64 {
    match llm.context_tokens() {
        Some(reported) if reported > 0 && est_tokens > 0 => {
            (reported as f64 / est_tokens as f64).clamp(CALIBRATION_RANGE.0, CALIBRATION_RANGE.1)
        }
        _ => 1.0,
    }
}

/// A dollar figure rounded to a tenth of a nanodollar, so sums of estimates
/// do not carry floating-point noise into the wire format.
fn round_usd(value: f64) -> f64 {
    (value * 1e10).round() / 1e10
}

/// The cache verdict for one call.
#[allow(clippy::too_many_arguments)]
fn cache_step(
    store: &Store,
    step: &TapeStep,
    llm: &LlmFacts,
    context: &Context,
    predecessor: std::result::Result<Option<(&Context, usize)>, (usize, &'static str)>,
    ttl_ns: u64,
    pool: &TextPool,
) -> CacheStep {
    let est_tokens = context.est_tokens();
    let mut previous_step_index = None;
    let mut gap_ns = None;
    let comparison = match (context.unavailable, predecessor) {
        (Some(reason), _) => Comparison {
            cause: Cause::Unknown,
            shared_chunks: 0,
            shared_est_tokens: 0,
            divergence: None,
            est_tokens_lost: 0,
            explanation: format!("this call's request {reason}, so its prefix cannot be compared"),
        },
        (None, Err((index, reason))) => Comparison {
            cause: Cause::Unknown,
            shared_chunks: 0,
            shared_est_tokens: 0,
            divergence: None,
            est_tokens_lost: 0,
            explanation: format!(
                "the request of step {index}, the nearest earlier call, {reason}, so this call \
                 cannot be paired with its predecessor"
            ),
        },
        (None, Ok(None)) => Comparison {
            cause: Cause::NewContext,
            shared_chunks: 0,
            shared_est_tokens: 0,
            divergence: None,
            est_tokens_lost: 0,
            explanation: "the first call of this conversation in the run: there was no earlier \
                          prompt to cache"
                .to_owned(),
        },
        (None, Ok(Some((previous, _)))) => {
            previous_step_index = Some(previous.step_index);
            let gap = context.start_ns.saturating_sub(previous.start_ns);
            gap_ns = Some(gap);
            let ratio = calibration(llm, est_tokens);
            autopsy::compare(previous, context, gap, ttl_ns, pool, ratio)
        }
    };
    let est_tokens_lost = comparison.est_tokens_lost;
    let est_cost_lost_usd = step.model.as_deref().and_then(|model| {
        let saving = store.pricing().rate(model)?.cache_read_saving_per_mtok()?;
        let cost = round_usd(est_tokens_lost as f64 * saving / 1_000_000.0);
        cost.is_finite().then_some(cost)
    });
    CacheStep {
        step_index: step.index,
        span: SpanRef {
            trace_id: step.trace_id.clone(),
            span_id: step.span_id.clone(),
        },
        model: step.model.clone(),
        previous_step_index,
        chunks: context.chunks.len(),
        est_tokens,
        prefix_chunks_shared: comparison.shared_chunks,
        shared_est_tokens: comparison.shared_est_tokens,
        first_divergent_chunk: comparison.divergence,
        cache_read: llm.cache_read_tokens,
        cache_write: llm.cache_creation_tokens,
        reported_hit: llm.cache_read_tokens.map(|read| read > 0),
        gap_ns,
        cause: comparison.cause,
        explanation: comparison.explanation,
        est_tokens_lost,
        est_cost_lost_usd,
    }
}

/// Whether a verdict and the provider's report contradict each other.
fn disagrees(step: &CacheStep) -> bool {
    let nothing_servable = step.previous_step_index.is_some()
        && (step.cause == Cause::TtlExpired || step.prefix_chunks_shared == 0);
    (step.cause == Cause::Stable && step.reported_hit == Some(false))
        || (nothing_servable && step.reported_hit == Some(true))
}

/// The totals over an autopsy's steps.
fn summarize(steps: &[CacheStep]) -> CacheSummary {
    let mut by_cause: BTreeMap<&'static str, usize> = [
        Cause::Stable,
        Cause::VolatileContent,
        Cause::Reordered,
        Cause::TtlExpired,
        Cause::NewContext,
        Cause::Unknown,
    ]
    .iter()
    .map(|cause| (cause.as_str(), 0))
    .collect();
    let mut breaks = 0;
    let mut est_tokens_lost = 0;
    let mut cost: Option<f64> = None;
    let mut unpriced_breaks = 0;
    let mut disagreements = 0;
    type Shape = (Cause, Option<Change>, Option<String>, Option<Volatile>);
    let mut shapes: Vec<(Shape, Culprit)> = Vec::new();
    for step in steps {
        *by_cause.entry(step.cause.as_str()).or_default() += 1;
        if disagrees(step) {
            disagreements += 1;
        }
        est_tokens_lost += step.est_tokens_lost;
        if let Some(lost) = step.est_cost_lost_usd {
            cost = Some(round_usd(cost.unwrap_or(0.0) + lost));
        }
        let is_break = step.previous_step_index.is_some() && step.cause != Cause::Stable;
        if !is_break {
            continue;
        }
        breaks += 1;
        if step.est_cost_lost_usd.is_none() {
            unpriced_breaks += 1;
        }
        let divergence = step.first_divergent_chunk.as_ref();
        let shape: Shape = (
            step.cause,
            divergence.map(|d| d.change),
            divergence.and_then(|d| d.role.clone()),
            divergence.and_then(|d| d.volatile),
        );
        match shapes.iter_mut().find(|(known, _)| *known == shape) {
            Some((_, culprit)) => {
                culprit.breaks += 1;
                culprit.est_tokens_lost += step.est_tokens_lost;
                if let Some(lost) = step.est_cost_lost_usd {
                    culprit.est_cost_lost_usd =
                        Some(round_usd(culprit.est_cost_lost_usd.unwrap_or(0.0) + lost));
                }
            }
            None => shapes.push((
                shape.clone(),
                Culprit {
                    cause: shape.0,
                    change: shape.1,
                    role: shape.2.clone(),
                    volatile: shape.3,
                    breaks: 1,
                    est_tokens_lost: step.est_tokens_lost,
                    est_cost_lost_usd: step.est_cost_lost_usd,
                    example_step_index: step.step_index,
                    example: divergence.and_then(|d| d.diff.clone()),
                    advice: advice(shape.0, shape.1, shape.3),
                },
            )),
        }
    }
    let mut culprits: Vec<Culprit> = shapes.into_iter().map(|(_, culprit)| culprit).collect();
    culprits.sort_by(|a, b| {
        b.est_tokens_lost
            .cmp(&a.est_tokens_lost)
            .then_with(|| b.breaks.cmp(&a.breaks))
            .then_with(|| a.example_step_index.cmp(&b.example_step_index))
    });
    culprits.truncate(10);
    CacheSummary {
        steps: steps.len(),
        breaks,
        by_cause,
        est_tokens_lost,
        est_cost_lost_usd: cost,
        unpriced_breaks,
        disagreements,
        disagreement_advice: (disagreements > 0).then_some(DISAGREEMENT_ADVICE),
        culprits,
    }
}

/// A chunk's source label for the composition.
fn source_label(
    piece: &Piece,
    introduced: Option<&Introduced>,
    call_names: &HashMap<&str, &str>,
) -> String {
    if piece.role.as_deref() == Some(TOOLS_ROLE) {
        return "tool definitions".to_owned();
    }
    match piece.kind {
        ChunkKind::System => "system".to_owned(),
        ChunkKind::User => "user".to_owned(),
        ChunkKind::Assistant => "assistant".to_owned(),
        ChunkKind::ToolCall => "tool calls".to_owned(),
        ChunkKind::Other => "other".to_owned(),
        ChunkKind::ToolResult => {
            let from_step = introduced
                .filter(|step| step.kind == StepKind::Tool)
                .map(|step| step.name.clone());
            let from_call = piece
                .tool_call_id
                .as_deref()
                .and_then(|id| call_names.get(id))
                .map(|name| (*name).to_owned());
            match from_step.or(from_call) {
                Some(name) => format!("tool:{name}"),
                None => "tool results (unattributed)".to_owned(),
            }
        }
    }
}

fn shares(rows: HashMap<String, (usize, u64)>, total: u64, fold_past: Option<usize>) -> Vec<Share> {
    let mut rows: Vec<Share> = rows
        .into_iter()
        .map(|(key, (chunks, est_tokens))| Share {
            key,
            chunks,
            est_tokens,
            share: if total == 0 {
                0.0
            } else {
                est_tokens as f64 / total as f64
            },
        })
        .collect();
    rows.sort_by(|a, b| {
        b.est_tokens
            .cmp(&a.est_tokens)
            .then_with(|| b.chunks.cmp(&a.chunks))
            .then_with(|| a.key.cmp(&b.key))
    });
    if let Some(limit) = fold_past {
        if rows.len() > limit {
            let rest = rows.split_off(limit - 1);
            let (chunks, est_tokens) = rest
                .iter()
                .fold((0, 0), |(c, t), row| (c + row.chunks, t + row.est_tokens));
            rows.push(Share {
                key: "other sources".to_owned(),
                chunks,
                est_tokens,
                share: if total == 0 {
                    0.0
                } else {
                    est_tokens as f64 / total as f64
                },
            });
        }
    }
    rows
}

impl Store {
    /// The stale threshold this store's X-Rays default to
    /// ([`crate::Config::xray_stale_steps`]).
    pub fn xray_stale_steps(&self) -> usize {
        self.config.xray_stale_steps.max(1)
    }

    /// The X-Ray of the model call `trace_id`/`span_id` under `tenant`
    /// (`None`: the operator view, which must find the ids in exactly one
    /// tenant). See the [module docs](self).
    ///
    /// Budgeted by [`crate::Config::query_deadline`] — one budget for the
    /// whole request, payload resolution included.
    pub fn xray(
        &self,
        tenant: Option<&str>,
        trace_id: &str,
        span_id: &str,
        options: XrayOptions,
    ) -> Result<std::result::Result<XRay, XrayMiss>> {
        let deadline = self.query_deadline(Instant::now());
        let (spans, _) = self.trace_spans_at(tenant, trace_id, deadline)?;
        let matching: Vec<&Span> = spans
            .iter()
            .filter(|span| span.span_id == span_id)
            .collect();
        let span = match matching.as_slice() {
            [] => return Ok(Err(XrayMiss::NoSpan)),
            [span] => (*span).clone(),
            many => return Ok(Err(XrayMiss::AmbiguousTenant(many.len()))),
        };
        drop(spans);
        let (facts, _, llm) = self.step_facts_detailed(&span);
        if facts.kind != StepKind::Llm {
            return Ok(Err(XrayMiss::NotAnLlmStep(facts.kind)));
        }
        let run = semconv::run_id(&span.attributes, &span.trace_id);
        let tape = self.run_tape(
            Some(&span.tenant),
            &run,
            MAX_TAPE_SPANS,
            MAX_TAPE_SPANS,
            deadline,
        )?;
        let paired = pair(&tape.steps, &tape.spans);
        let target = paired.iter().position(|entry| {
            entry.step.trace_id == span.trace_id && entry.step.span_id == span.span_id
        });
        // Past the read, the call is X-rayed alone, without its history.
        let fallback = match target {
            Some(_) => None,
            None => {
                let (facts, extras, llm) = self.step_facts_detailed(&span);
                Some((
                    crate::tape::tape_step(0, &span, facts, extras, llm),
                    span.clone(),
                ))
            }
        };
        let fallback_pair = fallback.as_ref().map(|(step, span)| Paired { step, span });
        let target_entry = match (target, fallback_pair.as_ref()) {
            (Some(index), _) => &paired[index],
            (None, Some(alone)) => alone,
            (None, None) => return Ok(Err(XrayMiss::NoSpan)),
        };
        let tenant_scope = span.tenant.clone();
        let mut resolver = Resolver::new(self, Some(&tenant_scope), deadline);
        let mut target_budget = ResolveBudget::new(MAX_READ_BYTES);
        let read = read_call(
            target_entry,
            &mut resolver,
            &mut target_budget,
            options.resolve,
            true,
        )?;
        let pieces = read.context.list;
        let contents: HashSet<u128> = pieces.iter().map(|piece| piece.content).collect();

        // The history, oldest first, up to and including the call.
        let mut history_budget = ResolveBudget::new(MAX_READ_BYTES);
        let mut provenance: HashMap<u128, usize> = HashMap::new();
        let mut provenance_partial = target.is_none();
        let mut call_steps: HashMap<String, usize> = HashMap::new();
        let mut first_said: HashMap<u64, usize> = HashMap::new();
        let mut said_overflow = false;
        let mut output_steps: Vec<usize> = Vec::new();
        let mut recent: VecDeque<Context> = VecDeque::new();
        let mut pool = TextPool::new(MAX_POOL_BYTES);
        let mut resolved_partial = !options.resolve;
        let target_index = target.unwrap_or(0);
        let history: &[Paired<'_>] = match target {
            Some(index) => &paired[..=index],
            None => &[],
        };
        let mut note = |digest: u128, index: usize, provenance: &mut HashMap<u128, usize>| {
            if provenance.contains_key(&digest) {
                return;
            }
            if provenance.len() < MAX_PROVENANCE_ENTRIES || contents.contains(&digest) {
                provenance.insert(digest, index);
            } else {
                provenance_partial = true;
            }
        };
        let mut say = |text: &str, index: usize, first_said: &mut HashMap<u64, usize>| {
            chunk::words(text, |word| {
                if first_said.contains_key(&word) {
                    return;
                }
                if first_said.len() < MAX_SAID_WORDS {
                    first_said.insert(word, index);
                } else {
                    said_overflow = true;
                }
            });
        };
        for (index, entry) in history.iter().enumerate() {
            Deadline::check(deadline, 0)?;
            match entry.step.kind {
                StepKind::Llm if index == target_index => {
                    for piece in pieces.iter().chain(read.response.iter()) {
                        note(piece.content, index, &mut provenance);
                    }
                    let mut said = false;
                    for piece in &read.response {
                        if matches!(piece.kind, ChunkKind::Assistant | ChunkKind::ToolCall) {
                            say(&piece.text, index, &mut first_said);
                            said = true;
                        }
                    }
                    if said {
                        output_steps.push(index);
                    }
                }
                StepKind::Llm => {
                    let call = read_call(
                        entry,
                        &mut resolver,
                        &mut history_budget,
                        options.resolve,
                        true,
                    )?;
                    let mut said = false;
                    for piece in call.context.list.iter().chain(call.response.iter()) {
                        note(piece.content, index, &mut provenance);
                    }
                    for piece in &call.response {
                        if matches!(piece.kind, ChunkKind::Assistant | ChunkKind::ToolCall) {
                            say(&piece.text, index, &mut first_said);
                            said = true;
                        }
                    }
                    if said {
                        output_steps.push(index);
                    }
                    for piece in &call.context.list {
                        pool.keep(piece);
                    }
                    recent.push_back(Context::new(
                        index,
                        &entry.span.tenant,
                        entry.span.start_time_ns,
                        &call.context.list,
                        call.unavailable,
                    ));
                    if recent.len() > RECENT_CONTEXTS {
                        recent.pop_front();
                    }
                }
                StepKind::Tool => {
                    let mut step = entry.step.clone();
                    if options.resolve {
                        if let Some(response) = step.response.as_mut() {
                            resolver.resolve(response, &mut history_budget)?;
                        }
                        if let Some(request) = step.request.as_mut() {
                            resolver.resolve(request, &mut history_budget)?;
                        }
                    }
                    if let Some(id) = step.detail.tool_call_id.clone() {
                        call_steps.entry(id).or_insert(index);
                    }
                    if let Some(piece) = step.response.as_ref().and_then(|response| {
                        chunk::value_piece(response, ChunkKind::ToolResult, None)
                    }) {
                        note(piece.content, index, &mut provenance);
                    }
                    if let Some(arguments) = &step.request {
                        // A tool's arguments are model output too.
                        say(&chunk::content_text(arguments), index, &mut first_said);
                        output_steps.push(index);
                    }
                }
                StepKind::Agent => {
                    let mut step = entry.step.clone();
                    for value in [step.request.as_mut(), step.response.as_mut()]
                        .into_iter()
                        .flatten()
                    {
                        if options.resolve {
                            resolver.resolve(value, &mut history_budget)?;
                        }
                        if let Some(piece) = chunk::value_piece(value, ChunkKind::Other, None) {
                            note(piece.content, index, &mut provenance);
                        }
                    }
                }
                _ => {}
            }
        }
        resolved_partial |= target_budget.unresolved || history_budget.unresolved;
        provenance_partial |= said_overflow;

        // Provenance, age and staleness per chunk.
        let steps: Vec<&TapeStep> = history.iter().map(|entry| entry.step).collect();
        let call_names: HashMap<&str, &str> = pieces
            .iter()
            .filter(|piece| piece.kind == ChunkKind::ToolCall)
            .filter_map(|piece| Some((piece.tool_call_id.as_deref()?, piece.name.as_deref()?)))
            .collect();
        let mut first_of: HashMap<u128, usize> = HashMap::new();
        let mut chunks = Vec::with_capacity(pieces.len());
        let mut by_kind: HashMap<String, (usize, u64)> = HashMap::new();
        let mut by_source: HashMap<String, (usize, u64)> = HashMap::new();
        let mut totals = Totals {
            chunks: pieces.len(),
            est_tokens: 0,
            reported_input_tokens: llm.prompt_tokens,
            reported_context_tokens: llm.context_tokens(),
            cache_read: llm.cache_read_tokens,
            cache_write: llm.cache_creation_tokens,
            stale_chunks: 0,
            stale_est_tokens: 0,
            duplicate_est_tokens: 0,
        };
        for (position, piece) in pieces.iter().enumerate() {
            let est_tokens = piece.est_tokens();
            totals.est_tokens += est_tokens;
            let by_content = provenance.get(&piece.content).copied();
            let by_call = piece
                .tool_call_id
                .as_deref()
                .filter(|_| piece.kind == ChunkKind::ToolResult)
                .and_then(|id| call_steps.get(id).copied());
            let origin = match (by_content, by_call) {
                (Some(content), Some(call)) if call < content => Some((call, "tool_call_id")),
                (Some(content), _) => Some((content, "content")),
                (None, Some(call)) => Some((call, "tool_call_id")),
                (None, None) => None,
            };
            let introduced_by = origin
                .filter(|(index, _)| *index < steps.len())
                .map(|(index, via)| introduced(steps[index], via));
            let age_steps = introduced_by
                .as_ref()
                .map(|step| target_index.saturating_sub(step.step_index));
            let stale = match (&introduced_by, age_steps) {
                (Some(step), Some(age))
                    if piece.kind == ChunkKind::ToolResult
                        && age > options.stale_steps
                        && !said_overflow =>
                {
                    let from = step.step_index;
                    let spoke_since = output_steps
                        .iter()
                        .any(|&index| index > from && index <= target_index);
                    let mut referenced = false;
                    chunk::words(&piece.text, |word| {
                        if let Some(&at) = first_said.get(&word) {
                            referenced |= at > from && at <= target_index;
                        }
                    });
                    spoke_since && !referenced
                }
                _ => false,
            };
            if stale {
                totals.stale_chunks += 1;
                totals.stale_est_tokens += est_tokens;
            }
            let duplicate_of = match first_of.get(&piece.content) {
                Some(&first) => {
                    totals.duplicate_est_tokens += est_tokens;
                    Some(first)
                }
                None => {
                    first_of.insert(piece.content, position);
                    None
                }
            };
            let kind_row = by_kind.entry(piece.kind.as_str().to_owned()).or_default();
            kind_row.0 += 1;
            kind_row.1 += est_tokens;
            let source_row = by_source
                .entry(source_label(piece, introduced_by.as_ref(), &call_names))
                .or_default();
            source_row.0 += 1;
            source_row.1 += est_tokens;
            chunks.push(Chunk {
                index: position,
                role: piece.role.clone(),
                kind: piece.kind,
                chars: piece.chars(),
                est_tokens,
                digest: chunk::digest_hex(piece.content),
                introduced_by,
                age_steps,
                stale,
                preview: chunk::clip(&piece.text, PREVIEW_CHARS),
                name: piece.name.clone(),
                tool_call_id: piece.tool_call_id.clone(),
                duplicate_of,
            });
        }

        // Duplicates.
        let mut groups: HashMap<u128, Vec<usize>> = HashMap::new();
        for (position, piece) in pieces.iter().enumerate() {
            groups.entry(piece.content).or_default().push(position);
        }
        let mut duplicates: Vec<Duplicate> = groups
            .into_iter()
            .filter(|(_, indices)| indices.len() > 1)
            .map(|(digest, indices)| {
                let first = &pieces[indices[0]];
                Duplicate {
                    digest: chunk::digest_hex(digest),
                    count: indices.len(),
                    est_tokens_each: first.est_tokens(),
                    preview: chunk::clip(&first.text, PREVIEW_CHARS),
                    indices,
                }
            })
            .collect();
        duplicates.sort_by(|a, b| {
            let wasted = |d: &Duplicate| d.est_tokens_each * (d.count as u64 - 1);
            wasted(b)
                .cmp(&wasted(a))
                .then_with(|| a.indices[0].cmp(&b.indices[0]))
        });
        duplicates.truncate(MAX_DUPLICATES);

        // The predecessor, what was lost since, and the cache verdict.
        for piece in &pieces {
            pool.keep(piece);
        }
        let current = Context::new(
            target_index,
            &span.tenant,
            span.start_time_ns,
            &pieces,
            read.unavailable,
        );
        let found = autopsy::predecessor(&current, recent.iter(), &pool);
        let mut previous = None;
        let mut dropped = Vec::new();
        let mut dropped_truncated = false;
        let mut edited = Vec::new();
        if let Ok(Some((before, shared))) = &found {
            let step = steps[before.step_index];
            previous = Some(PreviousRef {
                trace_id: step.trace_id.clone(),
                span_id: step.span_id.clone(),
                step_index: before.step_index,
                shared_chunks: *shared,
            });
            let mut seen: HashSet<u128> = HashSet::new();
            // This call's chunks the previous call did not have: where an
            // edited chunk would be found.
            let mut fresh: Vec<usize> = pieces
                .iter()
                .enumerate()
                .filter(|(_, piece)| !before.contents.contains(&piece.content))
                .map(|(position, _)| position)
                .collect();
            let mut attempts = 0;
            for (previous_index, kept) in before.chunks.iter().enumerate() {
                if contents.contains(&kept.content) || !seen.insert(kept.content) {
                    continue;
                }
                // Changed rather than lost: a near-identical chunk of the same
                // role and kind took its place.
                let old_text = pool.get(kept.exact);
                let partner = old_text.and_then(|old| {
                    fresh.iter().position(|&position| {
                        let piece = &pieces[position];
                        if piece.kind != kept.kind
                            || piece.role != kept.role
                            || attempts >= MAX_EDIT_COMPARISONS
                            || old.len().max(piece.text.len()) > MAX_EDIT_BYTES
                        {
                            return false;
                        }
                        attempts += 1;
                        autopsy::similar(old, &piece.text)
                    })
                });
                if let (Some(slot), Some(old)) = (partner, old_text) {
                    let index = fresh.remove(slot);
                    if edited.len() < MAX_DROPPED {
                        let (diff, volatile) = autopsy::byte_diff(old, &pieces[index].text);
                        edited.push(Edited {
                            index,
                            previous_index,
                            role: kept.role.clone(),
                            kind: kept.kind,
                            diff,
                            volatile,
                        });
                    }
                    continue;
                }
                if dropped.len() == MAX_DROPPED {
                    dropped_truncated = true;
                    break;
                }
                let instruction = matches!(kept.kind, ChunkKind::System | ChunkKind::User)
                    && kept.role.as_deref() != Some(TOOLS_ROLE);
                dropped.push(Dropped {
                    digest: chunk::digest_hex(kept.content),
                    role: kept.role.clone(),
                    kind: if instruction { "instruction" } else { "other" },
                    chunk_kind: kept.kind,
                    est_tokens: kept.est_tokens,
                    preview: pool
                        .get(kept.exact)
                        .map(|text| chunk::clip(text, PREVIEW_CHARS)),
                    introduced_by: provenance
                        .get(&kept.content)
                        .filter(|index| **index < steps.len())
                        .map(|index| introduced(steps[*index], "content")),
                });
            }
        }
        let mut cache = cache_step(
            self,
            target_entry.step,
            &llm,
            &current,
            found,
            options.ttl_seconds.saturating_mul(1_000_000_000),
            &pool,
        );
        if target.is_none() {
            // No history was read, so "no predecessor" would be a claim
            // about steps nobody looked at.
            cache.cause = Cause::Unknown;
            cache.explanation = "this call lies past the run's read limit, so the calls before \
                                 it were not read and its predecessor is unknown"
                .to_owned();
            cache.est_tokens_lost = 0;
            cache.est_cost_lost_usd = None;
        }
        let response = read
            .response
            .iter()
            .enumerate()
            .map(|(position, piece)| Chunk {
                index: position,
                role: piece.role.clone(),
                kind: piece.kind,
                chars: piece.chars(),
                est_tokens: piece.est_tokens(),
                digest: chunk::digest_hex(piece.content),
                introduced_by: None,
                age_steps: None,
                stale: false,
                preview: chunk::clip(&piece.text, PREVIEW_CHARS),
                name: piece.name.clone(),
                tool_call_id: piece.tool_call_id.clone(),
                duplicate_of: None,
            })
            .collect();
        let total = totals.est_tokens;
        // An answer finished past the budget is refused like one stopped at
        // it: the budget is the request's, not each loop's.
        Deadline::check(deadline, 0)?;
        Ok(Ok(XRay {
            run,
            tenant: span.tenant.clone(),
            span: SpanRef {
                trace_id: span.trace_id.clone(),
                span_id: span.span_id.clone(),
            },
            step_index: target,
            model: llm.model.clone(),
            provider: llm.provider.clone(),
            start_ns: span.start_time_ns,
            chunks,
            context_unavailable: read.unavailable,
            chunks_truncated: read.context.truncated,
            totals,
            composition: Composition {
                by_kind: shares(by_kind, total, None),
                by_source: shares(by_source, total, Some(MAX_SOURCES)),
            },
            previous,
            dropped_since_previous: dropped,
            dropped_truncated,
            edited_since_previous: edited,
            duplicates,
            cache,
            stale_after_steps: options.stale_steps,
            response,
            resolved_partial,
            provenance_partial,
        }))
    }

    /// The Cache Autopsy of `run_id` under `tenant` (the scoping of
    /// [`Store::get_run_in`]), or `None` when no span carries that run id.
    /// See [`autopsy`] for how each call is judged.
    ///
    /// Budgeted by [`crate::Config::query_deadline`], one budget for the
    /// whole request.
    pub fn cache_autopsy(
        &self,
        tenant: Option<&str>,
        run_id: &str,
        options: AutopsyOptions,
    ) -> Result<Option<CacheAutopsy>> {
        let deadline = self.query_deadline(Instant::now());
        let tape = self.run_tape(tenant, run_id, MAX_TAPE_SPANS, MAX_TAPE_SPANS, deadline)?;
        if tape.spans.is_empty() {
            return Ok(None);
        }
        let paired = pair(&tape.steps, &tape.spans);
        let mut resolver = Resolver::new(self, tenant, deadline);
        let mut budget = ResolveBudget::new(MAX_READ_BYTES);
        let mut pool = TextPool::new(MAX_POOL_BYTES);
        let mut recent: VecDeque<Context> = VecDeque::new();
        let mut steps = Vec::new();
        let mut truncated = tape.truncated;
        let ttl_ns = options.ttl_seconds.saturating_mul(1_000_000_000);
        for entry in paired
            .iter()
            .filter(|entry| entry.step.kind == StepKind::Llm)
        {
            Deadline::check(deadline, 0)?;
            if steps.len() == MAX_AUTOPSY_STEPS {
                truncated = true;
                break;
            }
            let read = read_call(entry, &mut resolver, &mut budget, options.resolve, false)?;
            for piece in &read.context.list {
                pool.keep(piece);
            }
            let context = Context::new(
                entry.step.index,
                &entry.span.tenant,
                entry.span.start_time_ns,
                &read.context.list,
                read.unavailable,
            );
            let llm = self.facts(entry.span);
            let found = autopsy::predecessor(&context, recent.iter(), &pool);
            steps.push(cache_step(
                self, entry.step, &llm, &context, found, ttl_ns, &pool,
            ));
            recent.push_back(context);
            if recent.len() > RECENT_CONTEXTS {
                recent.pop_front();
            }
        }
        let summary = summarize(&steps);
        Deadline::check(deadline, 0)?;
        Ok(Some(CacheAutopsy {
            run: run_id.to_owned(),
            tenant: tenant.map(str::to_owned),
            ttl_seconds: options.ttl_seconds,
            steps,
            summary,
            truncated,
            resolved_partial: budget.unresolved || !options.resolve,
            texts_partial: pool.overflowed,
        }))
    }
}

#[cfg(test)]
mod tests;
