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
//!   reported as edited, with the bytes that changed, not as lost; when that
//!   could not be checked (its text was not kept, or a bound was reached),
//!   or this call's context was not read whole (cut at the chunk bound, or
//!   partly unreadable), the loss is marked `unverified` rather than
//!   asserted.
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
//! two before pricing anything — when the estimate covers what the provider
//! counted (no media, no unrecorded tool definitions, nothing cut).
//!
//! **Bounds.** One request reads its run under the store's snapshot, erasure
//! and tenant rules ([`crate::run`]), inside the query deadline, and keeps
//! [`MAX_TAPE_SPANS`] of its spans — its first, or for an X-Ray of a call
//! past them the ones that end at the call; resolves at most
//! [`MAX_READ_BYTES`] of offloaded
//! payload content for the call itself and as much again for its history
//! (the comparison window first), or that much per call and
//! [`MAX_AUTOPSY_READ_BYTES`] across an autopsy; splits one context into at
//! most [`MAX_CONTEXT_CHUNKS`] chunks; keeps at most
//! [`MAX_PROVENANCE_ENTRIES`] content digests for provenance,
//! [`MAX_SAID_WORDS`] words for staleness and [`MAX_POOL_BYTES`] of the
//! comparison window's chunk text for diffs; compares at most
//! [`MAX_EDIT_COMPARE_BYTES`] of text pairing lost chunks with edited ones;
//! and an autopsy analyses at most [`MAX_AUTOPSY_STEPS`] model calls. Past
//! any bound the answer says so (`chunks_truncated`, `provenance_partial`,
//! `resolved_partial`, `texts_partial`, `unverified`, `*_truncated`) rather
//! than looking complete.
//!
//! **What was not read is not evidence.** A context cut at the chunk bound,
//! or holding a value that could not be resolved — its request, its tool
//! definitions, or for a delta-recorded call an earlier answer it continues
//! — is not the context the model saw. Content found in it is found; content
//! missing from it may lie in the part not read. So a loss resting on such
//! an absence is `unverified`, a cache verdict that what was not compared
//! could change is `unknown` with nothing counted lost, a partial context
//! never calibrates, and staleness is not judged on model output that could
//! not be read.

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
use crate::tape::{payload_reference, ResolveBudget, Resolver, RunTape, TapeStep, MAX_TAPE_SPANS};
use crate::{Deadline, Result, Span, Store};
use autopsy::{Comparison, Context, TextPool};
use chunk::{Piece, TOOLS_ROLE};

/// The attribute naming the conversation a model call belongs to (the
/// OpenTelemetry GenAI `gen_ai.conversation.id`). Calls that carry one pair
/// only with calls of the same conversation, and with the nearest earlier
/// one of it always — even when nothing they hold is shared, which is what
/// a compaction looks like.
pub const CONVERSATION_ATTRIBUTE: &str = "gen_ai.conversation.id";

/// The attribute that says a model call recorded only what it added to its
/// conversation: `true` means its `gen_ai.input.messages` holds the messages
/// sent since the conversation's previous call ([`CONVERSATION_ATTRIBUTE`]),
/// and its context is that call's context, then that call's output, then
/// these. The recorders ([`crate::import`]) write transcripts this way —
/// each call's whole context again would grow a session quadratically —
/// and mark the first call after a compaction `false`: it starts over.
pub const INPUT_DELTA_ATTRIBUTE: &str = "traza.input.delta";

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

/// Offloaded payload bytes resolved for one model call in an autopsy, for
/// the call under an X-Ray, and — separately — for an X-Ray's history.
/// Payload content is read to be chunked and digested, never returned, so
/// this bounds reading work rather than response size (which is why it is
/// not `--tape-max-bytes`, the cap on content a tape *returns*); the query
/// deadline bounds it too.
pub const MAX_READ_BYTES: usize = 64 * 1024 * 1024;

/// Offloaded payload bytes one autopsy resolves across all its calls.
pub const MAX_AUTOPSY_READ_BYTES: usize = 512 * 1024 * 1024;

/// Distinct chunk text held for diffs and previews at once: the comparison
/// window's and the examined call's.
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

/// Text bytes one X-Ray compares while pairing the previous call's lost
/// chunks with edited ones. Texts of any size are compared; this bounds the
/// total.
pub const MAX_EDIT_COMPARE_BYTES: usize = 64 * 1024 * 1024;

/// Dropped chunks one X-Ray lists at most, and edited ones.
const MAX_DROPPED: usize = 256;

/// Duplicate groups one X-Ray lists at most.
const MAX_DUPLICATES: usize = 64;

/// Text comparisons one X-Ray spends pairing dropped chunks with edited
/// ones, however small the texts.
const MAX_EDIT_COMPARISONS: usize = 4_096;

/// Culprits an autopsy's summary lists at most.
const MAX_CULPRITS: usize = 10;

/// Sources the composition names before folding the rest.
const MAX_SOURCES: usize = 12;

/// Earlier calls searched for a call's predecessor: the comparison window.
const RECENT_CONTEXTS: usize = 32;

/// Characters of a chunk preview.
const PREVIEW_CHARS: usize = 120;

/// Ancestors walked looking for the agent step a model call runs under.
const MAX_AGENT_DEPTH: usize = 64;

/// Conversations whose running context one request keeps while it rebuilds
/// delta-recorded calls ([`INPUT_DELTA_ATTRIBUTE`]); past it the least
/// recently continued one is dropped, and a later call of it is read as
/// unavailable rather than as a context it was not.
pub const MAX_THREADS: usize = 64;

/// Chunk text those running contexts hold at most, across conversations.
pub const MAX_THREAD_BYTES: usize = 64 * 1024 * 1024;

/// Conversations remembered as dropped, so a later call of one is not taken
/// for a conversation's first; past it, a delta call of any conversation
/// not being kept is read as unavailable.
const MAX_LOST_THREADS: usize = 4_096;

/// How a calibration ratio is clamped: an estimate off by more than four
/// times either way says the estimate is not about the same text.
const CALIBRATION_RANGE: (f64, f64) = (0.25, 4.0);

/// What one request may spend — the bounds of the module docs, gathered so a
/// test can shrink them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    /// [`MAX_READ_BYTES`].
    pub(crate) read_bytes: usize,
    /// [`MAX_AUTOPSY_READ_BYTES`].
    pub(crate) autopsy_read_bytes: usize,
    /// [`MAX_POOL_BYTES`].
    pub(crate) pool_bytes: usize,
    /// [`MAX_EDIT_COMPARE_BYTES`].
    pub(crate) compare_bytes: usize,
}

impl Limits {
    /// The documented bounds.
    pub(crate) const DEFAULT: Self = Self {
        read_bytes: MAX_READ_BYTES,
        autopsy_read_bytes: MAX_AUTOPSY_READ_BYTES,
        pool_bytes: MAX_POOL_BYTES,
        compare_bytes: MAX_EDIT_COMPARE_BYTES,
    };
}

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
    /// No span carries that run id in scope.
    NoRun,
    /// The run has no step at that index.
    NoStep {
        /// Steps the run has.
        steps: usize,
        /// Whether more steps may exist than were counted (never, now that
        /// the whole run is counted; kept for the wire).
        more: bool,
    },
    /// The span is a step of another kind; the X-Ray reads model calls.
    NotAnLlmStep(StepKind),
    /// The operator view found the ids (or the run) in this many tenants.
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
    /// Not proven lost. Either this call's context was not read whole — cut
    /// at [`MAX_CONTEXT_CHUNKS`] (`chunks_truncated`), or partly unreadable
    /// (`context_unavailable`) — so the chunk may sit in the part not read;
    /// or this call has a chunk of the same role and kind that could be
    /// this one edited, but the two were not compared — the old text was not
    /// kept, or the comparison bound was reached — so the chunk may have
    /// been changed rather than lost.
    pub unverified: bool,
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
    /// Its tape index; `null` only for a call X-rayed alone.
    pub step_index: Option<usize>,
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
    /// Its context, or its predecessor's, had more than
    /// [`MAX_CONTEXT_CHUNKS`] chunks, and the rest was not analysed. The
    /// comparison then covers the chunks both sides analysed: a divergence
    /// among them is reported where it is, but a verdict that what lies past
    /// the cut could change — the whole prefix held, a new turn, a rewrite,
    /// a removal — is `unknown` with nothing counted lost, and a verdict it
    /// cannot change counts lost tokens among the chunks compared only.
    pub chunks_truncated: bool,
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
    /// Estimated tokens the cache could have served and did not, scaled by
    /// [`Self::calibration`] when there is one.
    pub est_tokens_lost: u64,
    /// The provider's context count over the estimate (clamped 0.25–4), by
    /// which [`Self::est_tokens_lost`] was scaled; `null` when it was left
    /// unscaled — no predecessor, no provider count, or an estimate that does
    /// not cover what the provider counted (media, unrecorded tool
    /// definitions, a context cut short).
    pub calibration: Option<f64>,
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
            "A changing number (a counter, a balance, a step count, a template variable) sits \
             ahead of text that does not change. Move it to the end of the prompt."
        }
        (Cause::VolatileContent, Some(Change::Edited), _) => {
            "A chunk inside the cached prefix is rewritten between calls. Keep stable content \
             first and put what changes per call at the end."
        }
        (Cause::VolatileContent, Some(Change::Replaced), _) => {
            "A chunk inside the cached prefix is swapped for different content between calls \
             (a re-retrieved document, a regenerated status block) while what follows it stays. \
             Put content that changes per call after the stable history."
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
    /// Recurring break shapes, costliest first: by estimated cost when every
    /// shape is priced, else by estimated tokens lost.
    pub culprits: Vec<Culprit>,
    /// More shapes than the ten listed.
    pub culprits_truncated: bool,
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
    /// Some call's context, or its predecessor's, was cut at
    /// [`MAX_CONTEXT_CHUNKS`] chunks ([`CacheStep::chunks_truncated`]).
    pub chunks_truncated: bool,
    /// Some offloaded value — a request, tool definitions, or an answer a
    /// delta-recorded call continues from — could not be resolved (or
    /// `resolve=false`); a call whose context it left partial is `unknown`.
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
    /// Its tape index, counted over the whole run — a call past the run's
    /// first [`MAX_TAPE_SPANS`] spans is read with the spans that end at it;
    /// `null` only when the call is not among its run's spans, and is
    /// X-rayed alone.
    pub step_index: Option<usize>,
    /// The model it named.
    pub model: Option<String>,
    /// The provider it named.
    pub provider: Option<String>,
    /// When it began.
    pub start_ns: u64,
    /// The context window, in order.
    pub chunks: Vec<Chunk>,
    /// Why the context could not be read whole (`"was not recorded"`, `"was
    /// offloaded and could not be resolved"`, `"was offloaded and
    /// resolve=false"`, tool definitions or — for a call recorded as a delta
    /// of its conversation — an earlier call or its answer that could not be
    /// read, or a conversation that cannot be found), or `null` when it was.
    /// The chunks are then what could be read, a placeholder standing for
    /// each offloaded value that could not; the context is neither compared
    /// with its predecessor's nor used to calibrate, and a chunk the
    /// previous call had and this one lacks is `unverified`.
    pub context_unavailable: Option<&'static str>,
    /// The context had more than [`MAX_CONTEXT_CHUNKS`] chunks. The previous
    /// call's being cut too is in `cache.chunks_truncated`; only the chunks
    /// of it that were analysed are checked for losses.
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
    /// More was edited than listed.
    pub edited_truncated: bool,
    /// Repeated content.
    pub duplicates: Vec<Duplicate>,
    /// More content repeats than listed.
    pub duplicates_truncated: bool,
    /// The call's cache verdict.
    pub cache: CacheStep,
    /// The stale threshold applied.
    pub stale_after_steps: usize,
    /// The model's output, chunked the same way.
    pub response: Vec<Chunk>,
    /// Some offloaded value could not be resolved (or `resolve=false`).
    pub resolved_partial: bool,
    /// A provenance or staleness bound was reached, the history was cut
    /// short, or some of it could not be read (an offloaded value left
    /// unresolved), so `introduced_by` and `stale` are incomplete. When a
    /// model output could not be read, or too many words were said, nothing
    /// is called stale: a result it quoted would look unquoted.
    pub provenance_partial: bool,
    /// Some chunk text was not kept for lack of room: a preview or diff is
    /// `null`, and a lost chunk may be `unverified`.
    pub texts_partial: bool,
}

impl XRay {
    /// Adds `by` to every tape index the X-Ray names: it was read from a
    /// tape that begins `by` steps into its run.
    fn offset_steps(&mut self, by: usize) {
        let shift = |index: &mut usize| *index += by;
        if let Some(index) = self.step_index.as_mut() {
            shift(index);
        }
        if let Some(previous) = self.previous.as_mut() {
            shift(&mut previous.step_index);
        }
        for chunk in self.chunks.iter_mut().chain(self.response.iter_mut()) {
            if let Some(introduced) = chunk.introduced_by.as_mut() {
                shift(&mut introduced.step_index);
            }
        }
        for dropped in &mut self.dropped_since_previous {
            if let Some(introduced) = dropped.introduced_by.as_mut() {
                shift(&mut introduced.step_index);
            }
        }
        if let Some(index) = self.cache.step_index.as_mut() {
            shift(index);
        }
        if let Some(index) = self.cache.previous_step_index.as_mut() {
            shift(index);
        }
    }
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

/// A span's key within a run read: tenant, trace, span.
type SpanKey<'t> = (&'t str, &'t str, &'t str);

/// Which agent each model call belongs to, as far as the trace says: the
/// call's own `gen_ai.agent.name`, else the name of the nearest enclosing
/// agent step (at most [`MAX_AGENT_DEPTH`] levels up). Calls of two
/// differently named agents are never compared with each other.
struct Agents<'t> {
    parents: HashMap<SpanKey<'t>, &'t str>,
    names: HashMap<SpanKey<'t>, &'t str>,
}

impl<'t> Agents<'t> {
    fn new(steps: &'t [TapeStep], spans: &'t [Span]) -> Self {
        let parents = spans
            .iter()
            .filter_map(|span| {
                let parent = span.parent_span_id.as_deref()?;
                Some((
                    (
                        span.tenant.as_str(),
                        span.trace_id.as_str(),
                        span.span_id.as_str(),
                    ),
                    parent,
                ))
            })
            .collect();
        let names = steps
            .iter()
            .filter(|step| step.kind == StepKind::Agent)
            .filter_map(|step| {
                let name = step.agent.as_deref()?;
                Some((
                    (
                        step.detail.tenant.as_str(),
                        step.trace_id.as_str(),
                        step.span_id.as_str(),
                    ),
                    name,
                ))
            })
            .collect();
        Self { parents, names }
    }

    fn of(&self, span: &Span) -> Option<String> {
        if let Some(name) = span
            .attributes
            .get("gen_ai.agent.name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
        {
            return Some(name.to_owned());
        }
        let mut current = span.parent_span_id.as_deref()?;
        for _ in 0..MAX_AGENT_DEPTH {
            let key = (span.tenant.as_str(), span.trace_id.as_str(), current);
            if let Some(name) = self.names.get(&key) {
                return Some((*name).to_owned());
            }
            current = self.parents.get(&key).copied()?;
        }
        None
    }
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

/// Resolves `value` when `resolve` says to, returning whether a reference
/// stayed in it: what resolution left, or — not resolving — every reference
/// there is.
fn left_unread(
    value: &mut Value,
    resolver: &mut Resolver<'_>,
    budget: &mut ResolveBudget,
    resolve: bool,
) -> Result<bool> {
    if resolve {
        Ok(resolver.resolve(value, budget)?.any())
    } else {
        Ok(has_reference(value))
    }
}

/// Why a request whose offloaded messages stayed references cannot be read.
fn request_unread(resolve: bool) -> &'static str {
    if resolve {
        "was offloaded and could not be resolved"
    } else {
        "was offloaded and resolve=false"
    }
}

/// Why tool definitions that stayed references cannot be read; it reads
/// after "the request", as every reason does.
fn definitions_unread(resolve: bool) -> &'static str {
    if resolve {
        "carried tool definitions that were offloaded and could not be resolved"
    } else {
        "carried tool definitions that were offloaded, and resolve=false"
    }
}

/// A model call's context and output, read and chunked — and, part by
/// part, what of it could not be read. A part left an offloaded reference
/// is chunked as its placeholder, which is not the content the model saw,
/// so each part says so separately: its request, its tool definitions and
/// its answer, which a delta-recorded call after it continues from.
struct Read {
    context: chunk::Pieces,
    response: Vec<Piece>,
    /// Why its messages are not all there (its request, or for a
    /// delta-recorded call the context it continues), when they are not.
    unavailable: Option<&'static str>,
    /// Why its tool definitions are not, when they are not.
    definitions_unavailable: Option<&'static str>,
    /// Its answer was read but kept an offloaded reference unresolved.
    response_unresolved: bool,
    /// Some value of its own — request, tool definitions or answer — kept
    /// an offloaded reference unresolved, so what it held is not all known.
    unresolved: bool,
}

impl Read {
    /// Why the context as chunked is not the whole context the call
    /// carried, when it is not: such a context is neither compared whole nor
    /// used to calibrate.
    fn context_unavailable(&self) -> Option<&'static str> {
        self.unavailable.or(self.definitions_unavailable)
    }
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
    // What stayed a reference, part by part: what resolution left, or —
    // not resolving — every reference there is.
    let (request_left, definitions_left, response_left) = if resolve {
        let request = resolver.resolve_request(&mut step, budget)?.any();
        let mut definitions_left = false;
        for definition in &mut definitions {
            // As text first, so definitions recorded as text keep their
            // bytes; then anything nested.
            let mut left = resolver.resolve_text(definition, budget)?;
            left.merge(resolver.resolve(definition, budget)?);
            definitions_left |= left.any();
        }
        let response = match (with_response, step.response.as_mut()) {
            (true, Some(response)) => resolver.resolve(response, budget)?.any(),
            _ => false,
        };
        (request, definitions_left, response)
    } else {
        (
            step.request.as_ref().is_some_and(has_reference),
            definitions.iter().any(has_reference),
            with_response && step.response.as_ref().is_some_and(has_reference),
        )
    };
    let context = chunk::context_pieces(step.request.as_ref(), &definitions, MAX_CONTEXT_CHUNKS);
    let response = match (&step.response, with_response) {
        (Some(response), true) => chunk::response_pieces(response, MAX_CONTEXT_CHUNKS).list,
        _ => Vec::new(),
    };
    let unavailable = match &step.request {
        None => Some(NOT_RECORDED),
        Some(_) => request_left.then(|| request_unread(resolve)),
    };
    Ok(Read {
        context,
        response,
        unavailable,
        definitions_unavailable: definitions_left.then(|| definitions_unread(resolve)),
        response_unresolved: response_left,
        unresolved: request_left || definitions_left || response_left,
    })
}

/// The conversation a model call names ([`CONVERSATION_ATTRIBUTE`]), as its
/// identity: the SHA-256 of the whole id, in hex. An id is producer text of
/// any length, so it is neither cut short — two ids sharing a long prefix
/// would become one conversation, each continuing the other's — nor kept
/// whole, since a request remembers thousands; the digest is fixed in size
/// and collision-resistant. `None` when the call names none it can be read
/// by: absent, empty, offloaded or not text.
fn conversation_of(span: &Span) -> Option<String> {
    span.attributes
        .get(CONVERSATION_ATTRIBUTE)
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(|id| crate::payload::sha256_hex(id.as_bytes()))
}

/// Whether a model call recorded only what it added to its conversation
/// ([`INPUT_DELTA_ATTRIBUTE`]): the boolean `true`, or exactly `"true"`.
fn is_delta(span: &Span) -> bool {
    match span.attributes.get(INPUT_DELTA_ATTRIBUTE) {
        Some(Value::Bool(delta)) => *delta,
        Some(Value::String(delta)) => delta == "true",
        _ => false,
    }
}

/// Why a context cannot be read when its call recorded none.
const NOT_RECORDED: &str = "was not recorded";

/// Why a delta-recorded call's context could not be rebuilt from an earlier
/// call of its conversation that could not be read.
const EARLIER_UNREADABLE: &str =
    "could not be rebuilt: an earlier call of its conversation could not be read";

/// Why a delta-recorded call's context could not be rebuilt whole from an
/// earlier call whose answer could not be read.
const EARLIER_ANSWER_UNREADABLE: &str =
    "could not be rebuilt whole: the answer of an earlier call of its conversation was offloaded \
     and could not be resolved";

/// Why a delta-recorded call that names no conversation it can be read by
/// shows only part of its context.
const UNNAMED_DELTA: &str =
    "could not be rebuilt: it recorded only what it added, and names no conversation \
     (gen_ai.conversation.id) that can be read to find what it added to";

/// Why a delta-recorded call's context could not be rebuilt.
const NOT_REBUILT: &str =
    "could not be rebuilt: it recorded only what it added, and its conversation's earlier \
     context was not kept";

/// Why a delta-recorded call read from a tape that ends at a long run's
/// later call could not be rebuilt: its conversation began before the tape.
const BEGAN_BEFORE_READ: &str =
    "could not be rebuilt: it recorded only what it added, and its conversation began before \
     the 20,000 spans read for it (it is rebuilt from its next compaction on)";

/// Why a delta-recorded call X-rayed alone shows only part of its context.
const DELTA_ALONE: &str =
    "was recorded as a delta of its conversation, whose earlier calls were not found in its run";

/// One conversation's running context: its latest call's whole context and
/// what that call answered — each with what of it could not be read, which
/// the next call inherits along with the content.
struct Thread {
    context: Vec<Piece>,
    truncated: bool,
    unavailable: Option<&'static str>,
    definitions_unavailable: Option<&'static str>,
    response: Vec<Piece>,
    response_unresolved: bool,
    bytes: usize,
    used: u64,
}

/// The running contexts of the delta-recorded conversations of one read
/// ([`INPUT_DELTA_ATTRIBUTE`]), so a model call that recorded only what it
/// added to its conversation is read as the whole context it was: its
/// conversation's previous context, then what that call answered, then its
/// own messages. Fed every model call of a run in step order. Bounded by
/// [`MAX_THREADS`] conversations and [`MAX_THREAD_BYTES`] of their text.
#[derive(Default)]
struct Threads {
    running: HashMap<(String, String), Thread>,
    bytes: usize,
    clock: u64,
    /// Conversations dropped for room: a later delta call of one cannot be
    /// rebuilt.
    lost: HashSet<(String, String)>,
    /// Conversations already under way before the calls read began: a
    /// delta call of one cannot be rebuilt until it starts over.
    before: HashSet<(String, String)>,
    lost_overflow: bool,
}

impl Threads {
    /// Folds one call's read into its conversation, returning the read with
    /// its whole context. A call naming no conversation is returned as it
    /// is — unless it recorded only a delta, which then continues a context
    /// nothing can find, and says so; a delta call opening its conversation
    /// is returned as it is too, its delta being all there was.
    fn fold(&mut self, span: &Span, mut read: Read) -> Read {
        let Some(conversation) = conversation_of(span) else {
            if is_delta(span) {
                read.unavailable = Some(
                    read.unavailable
                        .filter(|reason| *reason != NOT_RECORDED)
                        .unwrap_or(UNNAMED_DELTA),
                );
            }
            return read;
        };
        let key = (span.tenant.clone(), conversation);
        self.clock += 1;
        if is_delta(span) {
            match self.running.get(&key) {
                Some(thread) => {
                    let is_definition = |piece: &&Piece| piece.role.as_deref() == Some(TOOLS_ROLE);
                    let own = &read.context.list;
                    let own_definitions = own.iter().take_while(is_definition).count();
                    let base_definitions = thread.context.iter().take_while(is_definition).count();
                    // Its own definitions when it recorded any, else its
                    // conversation's (and whether those could be read);
                    // then the conversation, the answer to its last call,
                    // and what this call added.
                    let definitions = if own_definitions > 0 {
                        &own[..own_definitions]
                    } else {
                        read.definitions_unavailable = thread.definitions_unavailable;
                        &thread.context[..base_definitions]
                    };
                    let mut list: Vec<Piece> = definitions
                        .iter()
                        .chain(&thread.context[base_definitions..])
                        .chain(&thread.response)
                        .chain(&own[own_definitions..])
                        .take(MAX_CONTEXT_CHUNKS + 1)
                        .cloned()
                        .collect();
                    let mut truncated = thread.truncated || read.context.truncated;
                    if list.len() > MAX_CONTEXT_CHUNKS {
                        list.truncate(MAX_CONTEXT_CHUNKS);
                        truncated = true;
                    }
                    read.context.list = list;
                    read.context.truncated = truncated;
                    // A delta that added nothing is not an unrecorded one;
                    // an unreadable earlier call — or an answer of it left
                    // a reference, which this context now carries as its
                    // placeholder — leaves this one partial.
                    let earlier = thread.unavailable.map(|_| EARLIER_UNREADABLE).or(thread
                        .response_unresolved
                        .then_some(EARLIER_ANSWER_UNREADABLE));
                    read.unavailable = match read.unavailable {
                        Some(NOT_RECORDED) => earlier,
                        own => own.or(earlier),
                    };
                }
                None if self.lost_overflow || self.lost.contains(&key) => {
                    read.unavailable = read.unavailable.or(Some(NOT_REBUILT));
                }
                None if self.before.contains(&key) => {
                    read.unavailable = read.unavailable.or(Some(BEGAN_BEFORE_READ));
                }
                None => {}
            }
        }
        self.keep(key, &read);
        read
    }

    /// Keeps `read` as its conversation's running context, dropping the
    /// least recently continued conversations past the bounds.
    fn keep(&mut self, key: (String, String), read: &Read) {
        if let Some(old) = self.running.remove(&key) {
            self.bytes -= old.bytes;
        }
        let bytes: usize = read
            .context
            .list
            .iter()
            .chain(&read.response)
            .map(|piece| piece.text.len())
            .sum();
        while !self.running.is_empty()
            && (self.running.len() >= MAX_THREADS
                || self.bytes.saturating_add(bytes) > MAX_THREAD_BYTES)
        {
            let oldest = self
                .running
                .iter()
                .min_by_key(|(_, thread)| thread.used)
                .map(|(key, _)| key.clone())
                .expect("not empty");
            if let Some(gone) = self.running.remove(&oldest) {
                self.bytes -= gone.bytes;
            }
            self.forget(oldest);
        }
        if bytes > MAX_THREAD_BYTES {
            self.forget(key);
            return;
        }
        self.lost.remove(&key);
        self.bytes += bytes;
        self.running.insert(
            key,
            Thread {
                context: read.context.list.clone(),
                truncated: read.context.truncated,
                unavailable: read.unavailable,
                definitions_unavailable: read.definitions_unavailable,
                response: read.response.clone(),
                response_unresolved: read.response_unresolved,
                bytes,
                used: self.clock,
            },
        );
    }

    fn forget(&mut self, key: (String, String)) {
        if self.lost.len() < MAX_LOST_THREADS {
            self.lost.insert(key);
        } else {
            self.lost_overflow = true;
        }
    }
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

/// The ratio of the provider's context count to the estimate, clamped —
/// `None` when either is missing or zero.
fn calibration(llm: &LlmFacts, est_tokens: u64) -> Option<f64> {
    match llm.context_tokens() {
        Some(reported) if reported > 0 && est_tokens > 0 => Some(
            (reported as f64 / est_tokens as f64).clamp(CALIBRATION_RANGE.0, CALIBRATION_RANGE.1),
        ),
        _ => None,
    }
}

/// A dollar figure rounded to a tenth of a nanodollar, so sums of estimates
/// do not carry floating-point noise into the wire format.
fn round_usd(value: f64) -> f64 {
    (value * 1e10).round() / 1e10
}

/// What a cache verdict is about: the call, and what the search for its
/// predecessor found.
struct Verdict<'c> {
    step: &'c TapeStep,
    /// The call's tape index; `None` for a call X-rayed alone.
    step_index: Option<usize>,
    llm: &'c LlmFacts,
    context: &'c Context,
    predecessor: std::result::Result<Option<(&'c Context, usize)>, (usize, &'static str)>,
    /// Whether any earlier model call of the tenant was read at all.
    seen_before: bool,
}

/// The cache verdict for one call.
fn cache_step(store: &Store, verdict: Verdict<'_>, ttl_ns: u64, pool: &TextPool) -> CacheStep {
    let Verdict {
        step,
        step_index,
        llm,
        context,
        predecessor,
        seen_before,
    } = verdict;
    let est_tokens = context.est_tokens();
    let chunks_truncated =
        context.truncated || matches!(&predecessor, Ok(Some((previous, _))) if previous.truncated);
    let mut previous_step_index = None;
    let mut gap_ns = None;
    let mut applied = None;
    let unpaired = |explanation: String| Comparison {
        cause: Cause::Unknown,
        shared_chunks: 0,
        shared_est_tokens: 0,
        divergence: None,
        est_tokens_lost: 0,
        explanation,
    };
    let comparison = match (context.unavailable, predecessor) {
        (Some(reason), _) => unpaired(format!(
            "this call's request {reason}, so its prefix cannot be compared"
        )),
        (None, Err((index, reason))) => unpaired(format!(
            "the request of step {index}, the nearest earlier call, {reason}, so this call \
             cannot be paired with its predecessor"
        )),
        (None, Ok(None)) => Comparison {
            cause: Cause::NewContext,
            explanation: if !seen_before {
                "the first model call of the run: there was no earlier prompt to cache".to_owned()
            } else if context.conversation.is_some() {
                format!(
                    "no earlier call among the previous {RECENT_CONTEXTS} belongs to this call's \
                     conversation (gen_ai.conversation.id): the first call of a new \
                     conversation (a sub-agent, say), or one whose previous call lies further \
                     back"
                )
            } else {
                format!(
                    "no earlier call among the previous {RECENT_CONTEXTS} shares this call's \
                     leading chunks or half of its chunks: a new conversation (a sub-agent, \
                     say), or one whose previous call lies further back"
                )
            },
            ..unpaired(String::new())
        },
        (None, Ok(Some((previous, _)))) => {
            previous_step_index = Some(previous.step_index);
            let gap = context.start_ns.saturating_sub(previous.start_ns);
            gap_ns = Some(gap);
            applied = context
                .calibratable
                .then(|| calibration(llm, est_tokens))
                .flatten();
            let compared =
                autopsy::compare(previous, context, gap, ttl_ns, pool, applied.unwrap_or(1.0));
            served_past_ttl(compared, llm, previous.reported_context)
        }
    };
    let est_tokens_lost = comparison.est_tokens_lost;
    let est_cost_lost_usd = step.model.as_deref().and_then(|model| {
        let saving = store.pricing().rate(model)?.cache_read_saving_per_mtok()?;
        let cost = round_usd(est_tokens_lost as f64 * saving / 1_000_000.0);
        cost.is_finite().then_some(cost)
    });
    CacheStep {
        step_index,
        span: SpanRef {
            trace_id: step.trace_id.clone(),
            span_id: step.span_id.clone(),
        },
        model: step.model.clone(),
        previous_step_index,
        chunks: context.chunks.len(),
        est_tokens,
        chunks_truncated,
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
        calibration: applied,
        est_cost_lost_usd,
    }
}

/// Of what the predecessor reported as its context, the share this call
/// must report reading from the cache for an expired-TTL verdict to yield
/// to the provider.
const SERVED_SHARE: f64 = 0.5;

/// An expired-TTL verdict the provider's own report refutes, made `stable`.
///
/// The TTL is an assumption (`ttl_seconds`, five minutes unless asked), not
/// something a trace records — and real producers run longer caches: Claude
/// Code's and Codex's outlive five minutes. When the predecessor's context
/// was carried intact and this call reports reading at least
/// [`SERVED_SHARE`] of what the predecessor reported as its context, both
/// counts from the provider's own tokenizer, the cache plainly held: the
/// prefix was served and nothing was lost. A smaller read — a long-lived
/// system prompt alone, say — leaves the verdict standing. Only the TTL
/// verdict yields: what the recording shows changed is never overruled.
fn served_past_ttl(
    comparison: autopsy::Comparison,
    llm: &LlmFacts,
    reported: Option<u64>,
) -> autopsy::Comparison {
    let (Some(read), Some(reported)) = (llm.cache_read_tokens, reported) else {
        return comparison;
    };
    if comparison.cause != Cause::TtlExpired
        || reported == 0
        || (read as f64) < reported as f64 * SERVED_SHARE
    {
        return comparison;
    }
    let explanation = format!(
        "{} assumed — yet the provider read {} tokens from its cache, of the {} the previous \
         call reported: the cache outlived the assumed TTL, and the prefix was served",
        comparison.explanation,
        autopsy::thousands(read),
        autopsy::thousands(reported)
    );
    autopsy::Comparison {
        cause: Cause::Stable,
        est_tokens_lost: 0,
        explanation,
        ..comparison
    }
}

/// Whether a verdict and the provider's report contradict each other: a
/// prefix the cache could have served was not, or a read was reported where
/// nothing could be served.
fn disagrees(step: &CacheStep) -> bool {
    if step.previous_step_index.is_none() {
        return false;
    }
    let servable = step.prefix_chunks_shared > 0 && step.cause != Cause::TtlExpired;
    match step.reported_hit {
        Some(false) => step.cause == Cause::Stable && servable,
        // A new turn on a one-chunk predecessor shares no whole chunk, yet a
        // provider may serve part of it: no contradiction.
        Some(true) => !servable && step.cause != Cause::Stable,
        None => false,
    }
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
                culprit.est_cost_lost_usd =
                    match (culprit.est_cost_lost_usd, step.est_cost_lost_usd) {
                        (Some(sum), Some(lost)) => Some(round_usd(sum + lost)),
                        // One unpriced break makes the shape's cost unknown.
                        _ => None,
                    };
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
                    example_step_index: step.step_index.unwrap_or(0),
                    example: divergence.and_then(|d| d.diff.clone()),
                    advice: advice(shape.0, shape.1, shape.3),
                },
            )),
        }
    }
    let mut culprits: Vec<Culprit> = shapes.into_iter().map(|(_, culprit)| culprit).collect();
    // A total order either way: by cost only when every shape has one.
    let all_priced = culprits
        .iter()
        .all(|culprit| culprit.est_cost_lost_usd.is_some());
    culprits.sort_by(|a, b| {
        let by_cost = if all_priced {
            let cost = |culprit: &Culprit| culprit.est_cost_lost_usd.unwrap_or(0.0);
            cost(b).total_cmp(&cost(a))
        } else {
            std::cmp::Ordering::Equal
        };
        by_cost
            .then_with(|| b.est_tokens_lost.cmp(&a.est_tokens_lost))
            .then_with(|| b.breaks.cmp(&a.breaks))
            .then_with(|| a.example_step_index.cmp(&b.example_step_index))
    });
    let culprits_truncated = culprits.len() > MAX_CULPRITS;
    culprits.truncate(MAX_CULPRITS);
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
        culprits_truncated,
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

/// Where a tool result in the examined context came from: the step that
/// held the same content (`content_step`, the earliest one), or the tool
/// step that answered its call id (`call_steps`, every tool step carrying
/// that id, in order).
///
/// A tool step holding the content wins. Otherwise the content was first
/// seen in a model call — the agent changed the text before the model saw
/// it — and the tool step that answered the call is the latest one carrying
/// its id before that call, since ids repeat when a call is retried or a
/// framework numbers calls per turn.
fn origin(
    content_step: Option<usize>,
    call_steps: Option<&[usize]>,
    steps: &[&TapeStep],
) -> Option<(usize, &'static str)> {
    let content_is_tool = content_step
        .and_then(|index| steps.get(index))
        .is_some_and(|step| step.kind == StepKind::Tool);
    if content_is_tool {
        return content_step.map(|index| (index, "content"));
    }
    let before = content_step.unwrap_or(usize::MAX);
    let answered =
        call_steps.and_then(|indices| indices.iter().rev().find(|&&index| index < before).copied());
    match (answered, content_step) {
        (Some(index), _) => Some((index, "tool_call_id")),
        (None, Some(index)) => Some((index, "content")),
        (None, None) => None,
    }
}

/// The call an X-Ray reads and its run, read once under the request's
/// deadline.
pub(crate) struct Target {
    span: Span,
    run: String,
    /// The run's first [`MAX_TAPE_SPANS`] spans, or — for a call past them
    /// — the [`MAX_TAPE_SPANS`] spans that end at it.
    tape: RunTape,
    /// Steps of the run before the tape's first: what its indices are
    /// offset by.
    step_offset: usize,
    /// The conversations (tenant, digest) that had a model call before the
    /// tape began: a delta call of one cannot be rebuilt until its
    /// conversation starts over inside the tape.
    earlier: HashSet<(String, String)>,
}

/// A tape placed around an X-Ray's call: the tape, the steps of the run
/// before it, and the conversations under way before it (as on [`Target`]).
type Placed = (RunTape, usize, HashSet<(String, String)>);

/// Spans examined between two deadline checks while counting the steps
/// before a tape that ends at its call.
const COUNT_CHECK_SPANS: usize = 256;

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
        match self.xray_target(tenant, trace_id, span_id, deadline)? {
            Ok(target) => self.xray_in(target, deadline, options, Limits::DEFAULT),
            Err(miss) => Ok(Err(miss)),
        }
    }

    /// The X-Ray of step `step` (a tape index) of `run_id` under `tenant` —
    /// the run read once, under one deadline, for both the lookup and the
    /// X-Ray. In the operator view (`None`) a run present in more than one
    /// tenant is refused as ambiguous, since its step numbering would not be
    /// the one the X-Ray reports.
    pub fn xray_step(
        &self,
        tenant: Option<&str>,
        run_id: &str,
        step: usize,
        options: XrayOptions,
    ) -> Result<std::result::Result<XRay, XrayMiss>> {
        let deadline = self.query_deadline(Instant::now());
        match self.xray_step_target(tenant, run_id, step, deadline)? {
            Ok(target) => self.xray_in(target, deadline, options, Limits::DEFAULT),
            Err(miss) => Ok(Err(miss)),
        }
    }

    /// The span `trace_id`/`span_id` and its run.
    pub(crate) fn xray_target(
        &self,
        tenant: Option<&str>,
        trace_id: &str,
        span_id: &str,
        deadline: Option<Deadline>,
    ) -> Result<std::result::Result<Target, XrayMiss>> {
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
        let (facts, _, _) = self.step_facts_detailed(&span);
        if facts.kind != StepKind::Llm {
            return Ok(Err(XrayMiss::NotAnLlmStep(facts.kind)));
        }
        let run = semconv::run_id(&span.attributes, &span.trace_id);
        let read = self.run_spans(Some(&span.tenant), &run, usize::MAX, deadline)?;
        let at = read
            .spans
            .iter()
            .position(|stored| stored.trace_id == span.trace_id && stored.span_id == span.span_id);
        let (tape, step_offset, earlier) = self.tape_around(read.spans, at, deadline)?;
        Ok(Ok(Target {
            span,
            run,
            tape,
            step_offset,
            earlier,
        }))
    }

    /// The tape an X-Ray of the call at position `at` of `spans` (a whole
    /// run, in step order) reads: the run's first [`MAX_TAPE_SPANS`] spans
    /// when the call lies among them — or when it is not among `spans` at
    /// all, and is X-rayed alone — else the [`MAX_TAPE_SPANS`] spans that
    /// end at the call, so a long session's latest calls are read with the
    /// history nearest them rather than alone. Returns the tape, the steps
    /// before it, and the conversations that had a model call before it.
    fn tape_around(
        &self,
        mut spans: Vec<Span>,
        at: Option<usize>,
        deadline: Option<Deadline>,
    ) -> Result<Placed> {
        let Some(at) = at.filter(|at| *at >= MAX_TAPE_SPANS) else {
            let truncated = spans.len() > MAX_TAPE_SPANS;
            spans.truncate(MAX_TAPE_SPANS);
            let tape = self.tape_of(spans, truncated, MAX_TAPE_SPANS, deadline)?;
            return Ok((tape, 0, HashSet::new()));
        };
        let start = at + 1 - MAX_TAPE_SPANS;
        let mut step_offset = 0;
        let mut earlier = HashSet::new();
        for (examined, span) in spans[..start].iter().enumerate() {
            if examined % COUNT_CHECK_SPANS == 0 {
                Deadline::check(deadline, 0)?;
            }
            let kind = semconv::step_kind_with(span, &self.facts(span));
            if kind.is_step() {
                step_offset += 1;
            }
            if kind == StepKind::Llm {
                if let Some(conversation) = conversation_of(span) {
                    earlier.insert((span.tenant.clone(), conversation));
                }
            }
        }
        spans.truncate(at + 1);
        let window = spans.split_off(start);
        drop(spans);
        let tape = self.tape_of(window, true, MAX_TAPE_SPANS, deadline)?;
        Ok((tape, step_offset, earlier))
    }

    /// Step `step` of `run_id` and its run.
    fn xray_step_target(
        &self,
        tenant: Option<&str>,
        run_id: &str,
        step: usize,
        deadline: Option<Deadline>,
    ) -> Result<std::result::Result<Target, XrayMiss>> {
        let read = self.run_spans(tenant, run_id, usize::MAX, deadline)?;
        if read.spans.is_empty() {
            return Ok(Err(XrayMiss::NoRun));
        }
        if tenant.is_none() {
            let tenants: HashSet<&str> =
                read.spans.iter().map(|span| span.tenant.as_str()).collect();
            if tenants.len() > 1 {
                return Ok(Err(XrayMiss::AmbiguousTenant(tenants.len())));
            }
        }
        // Step `step` of the whole run, wherever it lies.
        let mut seen = 0;
        let mut found = None;
        for (position, span) in read.spans.iter().enumerate() {
            if position % COUNT_CHECK_SPANS == 0 {
                Deadline::check(deadline, 0)?;
            }
            let kind = semconv::step_kind_with(span, &self.facts(span));
            if !kind.is_step() {
                continue;
            }
            if seen == step {
                found = Some((position, kind));
                break;
            }
            seen += 1;
        }
        let Some((at, kind)) = found else {
            return Ok(Err(XrayMiss::NoStep {
                steps: seen,
                more: false,
            }));
        };
        if kind != StepKind::Llm {
            return Ok(Err(XrayMiss::NotAnLlmStep(kind)));
        }
        let span = read.spans[at].clone();
        let (tape, step_offset, earlier) = self.tape_around(read.spans, Some(at), deadline)?;
        Ok(Ok(Target {
            span,
            run: run_id.to_owned(),
            tape,
            step_offset,
            earlier,
        }))
    }

    /// The X-Ray of `target`, spending `deadline` and at most `limits`.
    pub(crate) fn xray_in(
        &self,
        target: Target,
        deadline: Option<Deadline>,
        options: XrayOptions,
        limits: Limits,
    ) -> Result<std::result::Result<XRay, XrayMiss>> {
        let Target {
            span,
            run,
            tape,
            step_offset,
            earlier,
        } = target;
        let (_, _, llm) = self.step_facts_detailed(&span);
        let paired = pair(&tape.steps, &tape.spans);
        let agents = Agents::new(&tape.steps, &tape.spans);
        let target = paired.iter().position(|entry| {
            entry.step.trace_id == span.trace_id && entry.step.span_id == span.span_id
        });
        // Not among its run's spans (a run id its spans do not read back
        // to), the call is X-rayed alone, without its history.
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
        let mut target_budget = ResolveBudget::new(limits.read_bytes);

        let target_index = target.unwrap_or(0);
        let history: &[Paired<'_>] = match target {
            Some(index) => &paired[..=index],
            None => &[],
        };
        // The comparison window — the calls a predecessor can be — is read
        // first, newest first, so a long run spends its read budget on the
        // calls the verdict and the loss list are about; older history, read
        // only for provenance and staleness, gets what remains.
        let window: Vec<usize> = history[..history.len().saturating_sub(1)]
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.step.kind == StepKind::Llm)
            .map(|(index, _)| index)
            .rev()
            .take(RECENT_CONTEXTS)
            .collect();
        let mut history_budget = ResolveBudget::new(limits.read_bytes);
        let mut window_reads: HashMap<usize, Read> = HashMap::new();
        // A run whose calls recorded only what each added to its
        // conversation is read in step order instead, every call folded into
        // its conversation, so the call and the window are read as the whole
        // contexts they were; older calls are read again below, as recorded,
        // for provenance and staleness.
        let threaded = history
            .iter()
            .any(|entry| entry.step.kind == StepKind::Llm && is_delta(entry.span));
        let mut read = if threaded {
            let wanted: HashSet<usize> = window.iter().copied().collect();
            let mut threads = Threads::default();
            // A conversation already under way when the tape began has its
            // earlier calls outside it: a delta call of it cannot be rebuilt
            // until the conversation starts over (a compaction) in the tape.
            threads.before.extend(earlier);
            let mut target_read = None;
            for (index, entry) in history.iter().enumerate() {
                if entry.step.kind != StepKind::Llm {
                    continue;
                }
                Deadline::check(deadline, 0)?;
                let budget = if index == target_index {
                    &mut target_budget
                } else {
                    &mut history_budget
                };
                let call = read_call(entry, &mut resolver, budget, options.resolve, true)?;
                let call = threads.fold(entry.span, call);
                if index == target_index {
                    target_read = Some(call);
                } else if wanted.contains(&index) {
                    window_reads.insert(index, call);
                }
            }
            match target_read {
                Some(read) => read,
                None => read_call(
                    target_entry,
                    &mut resolver,
                    &mut target_budget,
                    options.resolve,
                    true,
                )?,
            }
        } else {
            let read = read_call(
                target_entry,
                &mut resolver,
                &mut target_budget,
                options.resolve,
                true,
            )?;
            for &index in &window {
                Deadline::check(deadline, 0)?;
                let call = read_call(
                    &history[index],
                    &mut resolver,
                    &mut history_budget,
                    options.resolve,
                    true,
                )?;
                window_reads.insert(index, call);
            }
            read
        };
        if target.is_none() && is_delta(&span) {
            read.unavailable = read.unavailable.or(Some(DELTA_ALONE));
        }
        let context_unavailable = read.context_unavailable();
        // Absence from this context is evidence of a loss only where it was
        // read whole: a chunk not found past the chunk bound, or in a part
        // that could not be read, may well be there. What is found is found
        // either way.
        let messages_whole = !read.context.truncated && read.unavailable.is_none();
        let definitions_whole = !read.context.truncated && read.definitions_unavailable.is_none();
        let pieces = read.context.list;
        let contents: HashSet<u128> = pieces.iter().map(|piece| piece.content).collect();
        // The call's own texts are held first and never released: its
        // previews and diffs matter most.
        let mut pool = TextPool::new(limits.pool_bytes);
        pool.hold(&pieces);

        // The history, oldest first, up to and including the call.
        let mut provenance: HashMap<u128, usize> = HashMap::new();
        // Past the run's beginning, the history before the tape is unread.
        let mut provenance_partial = target.is_none() || step_offset > 0;
        let mut call_steps: HashMap<String, Vec<usize>> = HashMap::new();
        let mut first_said: HashMap<u64, usize> = HashMap::new();
        let mut said_overflow = false;
        // A step held an offloaded value left unresolved: content first seen
        // there was not seen, so a chunk's introduction may read as later.
        let mut history_unread = false;
        // A model output (an answer, a tool's arguments) was left so: a
        // result it drew on would look unquoted, so staleness is not judged.
        let mut said_unread = read.response_unresolved;
        let mut output_steps: Vec<usize> = Vec::new();
        let mut recent: VecDeque<Context> = VecDeque::new();
        let mut resolved_partial = !options.resolve;
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
                    let in_window = window_reads.contains_key(&index);
                    let call = match window_reads.remove(&index) {
                        Some(call) => call,
                        None => read_call(
                            entry,
                            &mut resolver,
                            &mut history_budget,
                            options.resolve,
                            true,
                        )?,
                    };
                    history_unread |= call.unresolved;
                    said_unread |= call.response_unresolved;
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
                    if in_window {
                        let mut context = Context::new(
                            index,
                            &entry.span.tenant,
                            entry.span.start_time_ns,
                            &call.context.list,
                            call.context.truncated,
                            call.context_unavailable(),
                        )
                        .with_agent(agents.of(entry.span))
                        .with_conversation(conversation_of(entry.span))
                        .with_reported(self.facts(entry.span).context_tokens());
                        context.held = pool.hold(&call.context.list);
                        recent.push_back(context);
                    }
                }
                StepKind::Tool => {
                    let mut step = entry.step.clone();
                    let result_left = match step.response.as_mut() {
                        Some(response) => left_unread(
                            response,
                            &mut resolver,
                            &mut history_budget,
                            options.resolve,
                        )?,
                        None => false,
                    };
                    let arguments_left = match step.request.as_mut() {
                        Some(request) => left_unread(
                            request,
                            &mut resolver,
                            &mut history_budget,
                            options.resolve,
                        )?,
                        None => false,
                    };
                    history_unread |= result_left || arguments_left;
                    said_unread |= arguments_left;
                    if let Some(id) = step.detail.tool_call_id.clone() {
                        call_steps.entry(id).or_default().push(index);
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
                        history_unread |= left_unread(
                            value,
                            &mut resolver,
                            &mut history_budget,
                            options.resolve,
                        )?;
                        if let Some(piece) = chunk::value_piece(value, ChunkKind::Other, None) {
                            note(piece.content, index, &mut provenance);
                        }
                    }
                }
                _ => {}
            }
        }
        resolved_partial |= target_budget.unresolved || history_budget.unresolved;
        let said_partial = said_overflow || said_unread;
        provenance_partial |= said_partial || history_unread;

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
                .and_then(|id| call_steps.get(id))
                .map(Vec::as_slice);
            let introduced_by = origin(by_content, by_call, &steps)
                .filter(|(index, _)| *index < steps.len())
                .map(|(index, via)| introduced(steps[index], via));
            let age_steps = introduced_by
                .as_ref()
                .map(|step| target_index.saturating_sub(step.step_index));
            let stale = match (&introduced_by, age_steps) {
                (Some(step), Some(age))
                    if piece.kind == ChunkKind::ToolResult
                        && age > options.stale_steps
                        && !said_partial =>
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
        let duplicates_truncated = duplicates.len() > MAX_DUPLICATES;
        duplicates.truncate(MAX_DUPLICATES);

        // The predecessor, what was lost since, and the cache verdict.
        let current = Context::new(
            target_index,
            &span.tenant,
            span.start_time_ns,
            &pieces,
            read.context.truncated,
            context_unavailable,
        )
        .with_agent(agents.of(&span))
        .with_conversation(conversation_of(&span))
        .with_reported(llm.context_tokens());
        let found = autopsy::predecessor(&current, recent.iter(), &pool);
        let mut previous = None;
        let mut dropped = Vec::new();
        let mut dropped_truncated = false;
        let mut edited = Vec::new();
        let mut edited_truncated = false;
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
            let mut compare_budget = limits.compare_bytes;
            for (previous_index, kept) in before.chunks.iter().enumerate() {
                if contents.contains(&kept.content) || !seen.insert(kept.content) {
                    continue;
                }
                // Changed rather than lost: a near-identical chunk of the same
                // role and kind took its place. Lengths too far apart cannot
                // be an edit, which settles most candidates without a text.
                let old_text = pool.get(kept.exact);
                let mut partner = None;
                let mut unverified = if kept.role.as_deref() == Some(TOOLS_ROLE) {
                    !definitions_whole
                } else {
                    !messages_whole
                };
                for (slot, &position) in fresh.iter().enumerate() {
                    let piece = &pieces[position];
                    if piece.kind != kept.kind
                        || piece.role != kept.role
                        || !autopsy::could_be_edit(kept.bytes, piece.text.len())
                    {
                        continue;
                    }
                    let Some(old) = old_text else {
                        unverified = true;
                        break;
                    };
                    let cost = old.len().min(piece.text.len());
                    if attempts >= MAX_EDIT_COMPARISONS || cost > compare_budget {
                        unverified = true;
                        break;
                    }
                    attempts += 1;
                    compare_budget -= cost;
                    if autopsy::similar(old, &piece.text) {
                        partner = Some(slot);
                        break;
                    }
                }
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
                    } else {
                        edited_truncated = true;
                    }
                    continue;
                }
                if dropped.len() == MAX_DROPPED {
                    dropped_truncated = true;
                    continue;
                }
                let instruction = matches!(kept.kind, ChunkKind::System | ChunkKind::User)
                    && kept.role.as_deref() != Some(TOOLS_ROLE);
                dropped.push(Dropped {
                    digest: chunk::digest_hex(kept.content),
                    role: kept.role.clone(),
                    kind: if instruction { "instruction" } else { "other" },
                    chunk_kind: kept.kind,
                    est_tokens: kept.est_tokens,
                    preview: old_text.map(|text| chunk::clip(text, PREVIEW_CHARS)),
                    introduced_by: provenance
                        .get(&kept.content)
                        .filter(|index| **index < steps.len())
                        .map(|index| introduced(steps[*index], "content")),
                    unverified,
                });
            }
        }
        let seen_before = history[..history.len().saturating_sub(1)]
            .iter()
            .any(|entry| entry.step.kind == StepKind::Llm);
        let mut cache = cache_step(
            self,
            Verdict {
                step: target_entry.step,
                step_index: target,
                llm: &llm,
                context: &current,
                predecessor: found,
                seen_before,
            },
            options.ttl_seconds.saturating_mul(1_000_000_000),
            &pool,
        );
        if target.is_none() {
            // No history was read, so "no predecessor" would be a claim
            // about steps nobody looked at.
            cache.cause = Cause::Unknown;
            cache.explanation = "this call was not found among its run's spans, so the calls \
                                 before it were not read and its predecessor is unknown"
                .to_owned();
            cache.est_tokens_lost = 0;
            cache.calibration = None;
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
        let mut xray = XRay {
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
            context_unavailable,
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
            edited_truncated,
            duplicates,
            duplicates_truncated,
            cache,
            stale_after_steps: options.stale_steps,
            response,
            resolved_partial,
            provenance_partial,
            texts_partial: pool.overflowed,
        };
        if step_offset > 0 {
            xray.offset_steps(step_offset);
        }
        Ok(Ok(xray))
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
        self.autopsy_in(tenant, run_id, &tape, deadline, options, Limits::DEFAULT)
    }

    /// The autopsy of a run already read, spending `deadline` and at most
    /// `limits`.
    pub(crate) fn autopsy_in(
        &self,
        tenant: Option<&str>,
        run_id: &str,
        tape: &RunTape,
        deadline: Option<Deadline>,
        options: AutopsyOptions,
        limits: Limits,
    ) -> Result<Option<CacheAutopsy>> {
        if tape.spans.is_empty() {
            return Ok(None);
        }
        let paired = pair(&tape.steps, &tape.spans);
        let agents = Agents::new(&tape.steps, &tape.spans);
        let mut resolver = Resolver::new(self, tenant, deadline);
        let mut read_left = limits.autopsy_read_bytes;
        let mut unresolved = false;
        let mut pool = TextPool::new(limits.pool_bytes);
        let mut recent: VecDeque<Context> = VecDeque::new();
        let mut threads = Threads::default();
        let mut tenants_seen: HashSet<&str> = HashSet::new();
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
            // Each call gets its own budget, out of what the request has
            // left: one giant call cannot starve the calls after it.
            let mut budget = ResolveBudget::new(limits.read_bytes.min(read_left));
            // A call of a named conversation is read with its answer, which
            // the conversation's next call continues from when that call
            // recorded only what it added (see [`Threads`]).
            let conversation = conversation_of(entry.span);
            let read = read_call(
                entry,
                &mut resolver,
                &mut budget,
                options.resolve,
                conversation.is_some(),
            )?;
            let read = threads.fold(entry.span, read);
            read_left -= budget.spent();
            unresolved |= budget.unresolved;
            let llm = self.facts(entry.span);
            let mut context = Context::new(
                entry.step.index,
                &entry.span.tenant,
                entry.span.start_time_ns,
                &read.context.list,
                read.context.truncated,
                read.context_unavailable(),
            )
            .with_agent(agents.of(entry.span))
            .with_conversation(conversation)
            .with_reported(llm.context_tokens());
            context.held = pool.hold(&read.context.list);
            let seen_before = !tenants_seen.insert(entry.span.tenant.as_str());
            let found = autopsy::predecessor(&context, recent.iter(), &pool);
            steps.push(cache_step(
                self,
                Verdict {
                    step: entry.step,
                    step_index: Some(entry.step.index),
                    llm: &llm,
                    context: &context,
                    predecessor: found,
                    seen_before,
                },
                ttl_ns,
                &pool,
            ));
            recent.push_back(context);
            if recent.len() > RECENT_CONTEXTS {
                if let Some(gone) = recent.pop_front() {
                    pool.release(&gone.held);
                }
            }
        }
        let summary = summarize(&steps);
        let chunks_truncated = steps.iter().any(|step| step.chunks_truncated);
        Deadline::check(deadline, 0)?;
        Ok(Some(CacheAutopsy {
            run: run_id.to_owned(),
            tenant: tenant.map(str::to_owned),
            ttl_seconds: options.ttl_seconds,
            steps,
            summary,
            truncated,
            chunks_truncated,
            resolved_partial: unresolved || !options.resolve,
            texts_partial: pool.overflowed,
        }))
    }
}

#[cfg(test)]
mod tests;
