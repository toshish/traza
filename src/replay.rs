//! The replay proxy: a recorded run's model answers, served back to the agent
//! that produced them, with zero model calls.
//!
//! A replay is a snapshot of one run's tape ([`crate::tape`]) held in memory
//! under an id. The agent is pointed at it instead of at its provider — the
//! OpenAI SDK's base URL becomes `/v1/replays/{id}/openai/v1`, the Anthropic
//! SDK's `/v1/replays/{id}/anthropic` — and every chat completion or message
//! it asks for is answered with the NEXT recorded model step, rendered in that
//! provider's wire shape, streamed or not. Nothing leaves the process: Traza
//! never calls a model, and a replay is how that rule stays true while an
//! agent re-executes.
//!
//! # Matching
//!
//! Steps are served in occurrence order over the tape's llm steps. In
//! `strict` mode each incoming request is also compared against the recording:
//! its messages are normalised into exactly the shape step facts give a
//! recorded request (by running them through [`semconv::step_facts`] as an
//! OpenLLMetry-layout span), digested, and compared with the recorded step's
//! digest. A mismatch is a DIVERGENCE: the step is not consumed, the call is
//! refused with `409 replay_divergence`, and the replay remembers the first
//! one. `loose` mode serves in order regardless and records whether each call
//! matched, for the caller to inspect.
//!
//! The recorded side is normalised the same way, from its resolved form: a
//! request recorded as OTel `parts` messages, or as raw provider-shaped
//! messages, or with offloaded content, compares by what it SAID rather than
//! by how its instrumentation spelled it. For a request recorded inline in the
//! indexed layouts (OpenLLMetry, OpenInference) or as native events, the
//! normalised digest is exactly the tape's `request_digest`.
//!
//! # What a replay holds
//!
//! Only what it serves: each model step's answer and each tool step's
//! arguments and result, resolved from payload files where they were
//! offloaded, within `--tape-max-bytes`. A recorded REQUEST is resolved on its
//! own, digested and dropped — strict mode compares against the digest, so
//! keeping a long conversation's ever-growing prompts would spend the budget
//! on content nothing reads. A run longer than the span cap is refused rather
//! than replayed as its beginning.
//!
//! A replay is a copy taken out of the store, so erasure must reach it: every
//! call first checks whether an erasure recorded since the replay was created
//! covers any span, trace, session, tenant or payload it holds, and if one
//! does, the replay is withdrawn — its content dropped, every later call
//! refused `410 replay_erased`. The check is an epoch comparison when nothing
//! has been erased, so it costs a call nothing in the common case. Withdrawn
//! replays take no live replay's room, and are themselves bounded: past as
//! many as a share may hold live, the one closest to expiring is forgotten.
//!
//! # What a replay does not do
//!
//! - **Tools still execute live.** A replay answers model calls only. An agent
//!   whose tools have side effects will perform them again unless its harness
//!   takes recorded results from `tools/next` instead of running the tool.
//! - **Streaming is reconstructed**, from the stored text, in Traza's own
//!   chunking. The events are well-formed for the provider's SDK; they are not
//!   the provider's original byte-for-byte chunks, which no instrumentation
//!   records.
//! - **Concurrent calls are served in arrival order.** An agent that issues
//!   model calls in parallel gets the recorded steps in whatever order its
//!   requests reach the server; strict mode will report the swap as a
//!   divergence rather than silently mismatching answers to questions. Calls
//!   to one replay take turns, from reading its cursor to committing it, so
//!   each served step is rendered once however many calls arrive together.
//!
//! The registry is bounded twice — per tenant ([`MAX_TENANT_REPLAYS`] replays,
//! [`MAX_TENANT_BYTES`] of memory), so one credential cannot fill it for
//! everybody, and as a whole ([`MAX_LIVE_REPLAYS`], [`MAX_REGISTRY_BYTES`]) as
//! the backstop behind every share. A replay's bytes are everything it
//! retains, counted generously and charged while its snapshot is built, so a
//! run too large for a share is refused before it is all in memory. Each
//! replay lives [`REPLAY_TTL`] from creation, in memory only: a restart
//! forgets every replay, and creating one again is a read of the store.
//! Nothing is consumed past that expiry. A call is judged by the time it
//! reaches the replay, not the time its request arrived — once more when its
//! turn comes and again when it commits — so a call that waited past the
//! expiry is refused without rendering, and a rendering that finishes past
//! it is dropped unserved.
//!
//! A served answer is bounded too: no rendered response exceeds
//! [`MAX_RESPONSE_BYTES`], and the identifiers a stream repeats in every
//! chunk — the model and the response id — are served only up to
//! [`MAX_IDENTIFIER_BYTES`], so no request can make the server buffer a
//! multiple of its own size.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::erasure::{EraseRecord, Subject};
use crate::fork::{EditValue, ForkInfo, ForkSpec, MAX_EDITS};
use crate::semconv::{self, ResponseSource, StepKind};
use crate::tape::{self, Leftover, ResolveBudget, Resolver, TapeStep};
use crate::{Deadline, Span, Store};

/// How long a replay lives, counted from creation. Fixed rather than renewed
/// by use, so when a replay expires is a fact of its creation response.
pub const REPLAY_TTL: Duration = Duration::from_secs(60 * 60);

/// Replays held at once across every tenant. Past it, creation is refused
/// until one expires — never by evicting a live replay, which would break
/// somebody's run halfway.
pub const MAX_LIVE_REPLAYS: usize = 1024;

/// Memory the registry's live replays hold together: every allocation each
/// one retains — answers, tool records, identifiers, what erasure is checked
/// against, its call log at its full bound — with the collections' overhead,
/// counted generously rather than measured.
///
/// The count bound alone is not a memory bound: one replay of a long run with
/// large completions can be thousands of times another. Half a gibibyte is
/// generous for replaying agent runs and still a number an operator can plan
/// a process around.
pub const MAX_REGISTRY_BYTES: usize = 512 * 1024 * 1024;

/// Replays one tenant's data may hold at once — the same for the operator
/// view, which counts as a share of its own.
///
/// Any credential may create a replay (it is a read), so without a share one
/// read-only token could fill the registry and refuse every other tenant for
/// an hour. A sixteenth of the registry is far more than one harness drives
/// at a time, and sixteen tenants can hold full shares before the registry
/// itself is what refuses.
pub const MAX_TENANT_REPLAYS: usize = 64;

/// Memory one tenant's replays may hold together, counted as
/// [`MAX_REGISTRY_BYTES`] counts it.
///
/// An eighth of the registry rather than a sixteenth, because a share is also
/// the largest single replay that can ever exist: a long run with large
/// answers has to fit in it whole.
pub const MAX_TENANT_BYTES: usize = 64 * 1024 * 1024;

/// The largest response one replayed call renders, streamed or not.
///
/// A stream repeats fixed fields in every chunk and splits tool-call
/// arguments into many, so its size is not the recording's: an answer of a
/// thousand tiny tool calls streams to hundreds of times its recorded bytes.
/// The bound is the server's own request-body bound, and far above any
/// answer a model gives; a step that would render larger is refused, and not
/// consumed, before more than this is buffered.
pub const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// The longest model name or response id a replay serves.
///
/// Both are repeated in every chunk of an OpenAI stream, so one long enough
/// would turn a small answer into gigabytes — and when the recording names no
/// model, the request's own model is what would be echoed. No provider's
/// model names or ids come near this; a longer one, recorded or requested, is
/// served as if absent (an invented id, `unknown` for the model).
pub const MAX_IDENTIFIER_BYTES: usize = 256;

/// Calls one replay records for `GET /v1/replays/{id}`. A client stuck
/// retrying a divergence must not grow a replay without bound; past this the
/// log stops growing and serving continues.
const MAX_RECORDED_CALLS: usize = 4096;

/// Characters of each side a divergence's first difference quotes
/// ([`Registry::explain_divergence`]).
const DIFFERENCE_CHARS: usize = 120;

/// Characters a quoted difference starts before the first differing
/// character, so a reader sees where it sits.
const DIFFERENCE_CONTEXT: usize = 40;

/// What a recorded first difference may hold at most, charged up front with
/// the rest of a replay's bookkeeping: two quotes of [`DIFFERENCE_CHARS`]
/// characters (up to four bytes each) and the fields around them.
const DIFFERENCE_BYTES: usize = 2 * 4 * (DIFFERENCE_CHARS + 2) + 1024;

/// Characters per streamed delta window. Large texts use larger windows so
/// that an answer stays a bounded number of events (see [`deltas`]).
const STREAM_DELTA_CHARS: usize = 16;

/// Windows one streamed text or argument string is divided into; with cuts
/// moved back to whitespace, at most twice this many deltas.
const MAX_STREAM_DELTAS: usize = 256;

/// The attribute prefix the normaliser writes messages under. OpenLLMetry's
/// indexed layout, because it is the one whose reassembly
/// ([`semconv::step_facts`]) gives every field its own value rule.
const PROMPT_PREFIX: &str = "gen_ai.prompt.";

/// How strictly incoming requests must match the recording.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Each request's messages must digest to the recorded step's; a mismatch
    /// is refused as a divergence.
    Strict,
    /// Steps are served in order whatever was asked; matches are recorded.
    Loose,
}

impl Mode {
    /// Parses `strict` or `loose`.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "strict" => Some(Self::Strict),
            "loose" => Some(Self::Loose),
            _ => None,
        }
    }

    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::Loose => "loose",
        }
    }
}

/// Which provider's wire a request to a replay was shaped for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    /// OpenAI chat completions.
    OpenAi,
    /// Anthropic messages.
    Anthropic,
}

impl Provider {
    /// The normalised digest of a request body in this provider's shape
    /// ([`openai_request_digest`], [`anthropic_request_digest`]).
    pub fn request_digest(self, body: &Value) -> Result<Option<String>, String> {
        match self {
            Self::OpenAi => openai_request_digest(body),
            Self::Anthropic => anthropic_request_digest(body),
        }
    }

    fn messages(self, body: &Value) -> Result<Vec<Normalised>, String> {
        match self {
            Self::OpenAi => openai_messages(body),
            Self::Anthropic => anthropic_messages(body),
        }
    }
}

/// The bounds one [`Registry`] enforces. [`Limits::default`] is the
/// documented set; tests lower them to reach the edges cheaply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// How long a replay lives from creation.
    pub ttl: Duration,
    /// Replays held at once, across every tenant.
    pub max_live: usize,
    /// Recorded content held at once, across every tenant.
    pub max_bytes: usize,
    /// Replays one tenant's data (or the operator view) may hold at once.
    pub max_live_per_tenant: usize,
    /// Recorded content one tenant's replays may hold together — and so the
    /// largest replay that can ever be created.
    pub max_bytes_per_tenant: usize,
    /// Spans a replayed run may have. A longer run is refused, never replayed
    /// as its beginning.
    pub max_spans: usize,
    /// The largest response one call renders.
    pub max_response_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            ttl: REPLAY_TTL,
            max_live: MAX_LIVE_REPLAYS,
            max_bytes: MAX_REGISTRY_BYTES,
            max_live_per_tenant: MAX_TENANT_REPLAYS,
            max_bytes_per_tenant: MAX_TENANT_BYTES,
            max_spans: tape::MAX_TAPE_SPANS,
            max_response_bytes: MAX_RESPONSE_BYTES,
        }
    }
}

/// Why a replay operation was refused.
#[derive(Debug)]
pub enum ReplayError {
    /// No live replay has that id — or it belongs to a tenant the caller is
    /// not bound to, which answers the same way: whether another tenant's
    /// replay exists is not this caller's to learn.
    NotFound,
    /// The run has no spans under the caller's tenant scope.
    RunNotFound,
    /// No room for the replay now; there will be once replays expire.
    Full {
        /// `true` when the share of the tenant whose data the replay would
        /// hold is what is full; `false` when it is the registry as a whole.
        own_share: bool,
        /// The share's (or registry's) replay bound.
        max_live: usize,
        /// The share's (or registry's) content bound, in bytes.
        max_bytes: usize,
        /// Seconds until enough replays expire to make room — computed over
        /// the bound that refused, so it never promises room an expiry would
        /// not free.
        retry_after_seconds: u64,
    },
    /// The snapshot is larger than one tenant's share of the registry, so it
    /// can never be held, however long the caller waits.
    TooLarge {
        /// What the snapshot held when it passed the share — at least this
        /// many bytes, since building it stops there.
        bytes: usize,
        /// The most one tenant's replays may hold together.
        limit: usize,
    },
    /// The next recorded answer would render to more than
    /// [`Limits::max_response_bytes`] in the asked-for shape. The step is not
    /// consumed.
    ResponseTooLarge {
        /// Its tape index.
        step: usize,
        /// The bound it would pass.
        limit: usize,
    },
    /// The run cannot be replayed as asked; the message says which step and
    /// why, and what would make it replayable.
    Unreplayable(String),
    /// A request body this route cannot read.
    Invalid(String),
    /// Strict mode: the request does not match the recorded step.
    Diverged {
        /// The tape index of the step that was expected.
        step: usize,
        /// The recorded request's normalised digest.
        expected_digest: Option<String>,
        /// The incoming request's normalised digest.
        got_digest: Option<String>,
    },
    /// Every recorded llm step has been served.
    Exhausted {
        /// Llm steps served.
        served: usize,
    },
    /// No unserved tool step (of the asked-for name) remains.
    ToolsExhausted {
        /// The tool name asked for, if any.
        tool: Option<String>,
    },
    /// The next tool step's arguments or result could not be snapshotted, so
    /// there is no recorded value to serve. The step is not consumed.
    ToolUnreadable {
        /// Its tape index.
        step: usize,
        /// Its tool name, when recorded.
        tool: Option<String>,
        /// Why its value is missing.
        reason: String,
    },
    /// An erasure recorded after the replay was created covers content the
    /// replay held; the replay has been withdrawn.
    Erased {
        /// The erasure's id in the tombstone log.
        erasure: u64,
    },
    /// A fork's live point: the recording is not served after step
    /// `live_after`, so this call belongs to the real provider (or, from
    /// `tools/next`, the real tool). Nothing is consumed.
    Live {
        /// The fork's live point.
        live_after: usize,
        /// Llm steps the replay served before it.
        served: usize,
        /// The fork's id.
        fork_id: Option<String>,
    },
    /// The store refused the read the replay needed.
    Engine(crate::Error),
}

impl From<crate::Error> for ReplayError {
    fn from(error: crate::Error) -> Self {
        Self::Engine(error)
    }
}

impl fmt::Display for ReplayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str(
                "no live replay has this id; replays expire an hour after creation and do not \
                 survive a restart",
            ),
            Self::RunNotFound => f.write_str("run not found"),
            Self::Full {
                own_share: true,
                max_live,
                max_bytes,
                ..
            } => write!(
                f,
                "replays of this tenant's data already hold its share of the registry \
                 ({max_live} live replays, or {} of recorded content between them); retry \
                 once one of them expires",
                size(*max_bytes)
            ),
            Self::Full {
                own_share: false, ..
            } => f.write_str("the server's replay registry is full; retry once a replay expires"),
            Self::TooLarge { bytes, limit } => write!(
                f,
                "this run's replay would hold at least {} of recorded answers, tool records \
                 and bookkeeping, more than the {} one tenant's replays may hold together, so \
                 it can never be created; retrying will not help",
                size(*bytes),
                size(*limit)
            ),
            Self::ResponseTooLarge { step, limit } => write!(
                f,
                "recorded step {step} would render to more than {} in this shape, the most one \
                 replayed response may be; the step is not consumed",
                size(*limit)
            ),
            Self::Unreplayable(reason) | Self::Invalid(reason) => f.write_str(reason),
            Self::Diverged { step, .. } => write!(
                f,
                "this request's messages do not match recorded step {step}: the agent \
                 diverged from the recording, so replaying the recorded answer would answer \
                 a different question"
            ),
            Self::Exhausted { served } => {
                write!(f, "all {served} recorded model calls have been served")
            }
            Self::ToolsExhausted { tool: Some(name) } => {
                write!(f, "no unserved recorded tool step named {name} remains")
            }
            Self::ToolsExhausted { tool: None } => {
                f.write_str("no unserved recorded tool step remains")
            }
            Self::ToolUnreadable { step, tool, reason } => write!(
                f,
                "recorded tool step {step}{} cannot be served: {reason}",
                tool.as_deref()
                    .map(|name| format!(" ({name})"))
                    .unwrap_or_default()
            ),
            Self::Erased { erasure } => write!(
                f,
                "the recording this replay held was erased (erasure {erasure}) after the \
                 replay was created; the replay is withdrawn and serves nothing more"
            ),
            Self::Live { live_after, .. } => write!(
                f,
                "this fork goes live after step {live_after}: the recording is not served past \
                 it, so send this call to the real provider (or run the real tool) — the run \
                 continues from here as a new branch"
            ),
            Self::Engine(error) => write!(f, "the replay could not be served: {error}"),
        }
    }
}

impl std::error::Error for ReplayError {}

/// A byte count for a message: whole mebibytes when it is one, else bytes.
fn size(bytes: usize) -> String {
    const MIB: usize = 1024 * 1024;
    if bytes >= MIB && bytes % MIB == 0 {
        format!("{} MiB", bytes / MIB)
    } else {
        format!("{bytes} bytes")
    }
}

/// A replay as created: what the creation response reports.
#[derive(Clone, Debug, PartialEq)]
pub struct Created {
    /// The replay id, `rp_<hex>`.
    pub id: String,
    /// The run it replays.
    pub run: String,
    /// Steps on the run's tape: llm, tool and agent.
    pub steps: usize,
    /// Llm steps the replay will serve.
    pub llm_steps: usize,
    /// The matching mode.
    pub mode: Mode,
    /// When the replay expires, Unix nanoseconds.
    pub expires_at_ns: u64,
    /// Tape indices of tool steps whose arguments or result could not be
    /// snapshotted; `tools/next` refuses them rather than serving a payload
    /// reference as if it were the recorded value.
    pub unreadable_tool_steps: Vec<usize>,
    /// When the replay is a fork — created with edits or a live point
    /// ([`crate::fork`]) — what kind of fork.
    pub fork: Option<ForkInfo>,
}

impl Created {
    /// The `201` body of `POST /v1/replays`. A fork adds `fork_id` and
    /// `fork` ([`ForkInfo::to_json`]).
    pub fn to_json(&self) -> Value {
        let mut body = json!({
            "id": self.id,
            "run": self.run,
            "steps": self.steps,
            "llm_steps": self.llm_steps,
            "mode": self.mode.as_str(),
            "expires_at_ns": self.expires_at_ns,
            "unreadable_tool_steps": self.unreadable_tool_steps,
            "base_urls": {
                "openai": format!("/v1/replays/{}/openai/v1", self.id),
                "anthropic": format!("/v1/replays/{}/anthropic", self.id),
            },
        });
        if let Some(fork) = &self.fork {
            body["fork_id"] = json!(fork.id);
            body["fork"] = fork.to_json();
        }
        body
    }
}

/// One recorded model answer, ready to be served in either provider's shape.
#[derive(Clone, Debug, PartialEq)]
pub struct ServedStep {
    /// The replay that served it.
    replay: String,
    /// Its tape index.
    pub step: usize,
    model: Option<String>,
    response_id: Option<String>,
    start_ns: u64,
    choices: Vec<Choice>,
    tokens: tape::StepTokens,
    /// The most bytes a rendering may take ([`Limits::max_response_bytes`]).
    max_response_bytes: usize,
}

/// A recorded tool step, as `tools/next` returns it.
#[derive(Clone, Debug, PartialEq)]
pub struct ServedTool {
    /// Its tape index.
    pub step: usize,
    /// The tool's name, when the span recorded one.
    pub tool: Option<String>,
    /// The recorded arguments, `null` when none were captured.
    pub arguments: Value,
    /// The recorded result — `null` or `""` only when it was recorded so. A
    /// step whose result was never captured is refused, not served.
    pub result: Value,
    /// Whether `result` is a fork's edit rather than the recording's.
    pub edited: bool,
}

impl ServedTool {
    /// The `tools/next` response body. A fork's edited result adds
    /// `"edited": true`.
    pub fn to_json(&self) -> Value {
        let mut body = json!({
            "step": self.step,
            "tool": self.tool,
            "arguments": self.arguments,
            "result": self.result,
        });
        if self.edited {
            body["edited"] = json!(true);
        }
        body
    }
}

/// One choice of a recorded answer: its text, the tool calls it made, and how
/// it finished.
#[derive(Clone, Debug, Default, PartialEq)]
struct Choice {
    text: Option<String>,
    calls: Vec<Call>,
    finish_reason: Option<String>,
}

/// One recorded tool call.
#[derive(Clone, Debug, PartialEq)]
struct Call {
    id: Option<String>,
    name: String,
    /// As recorded: an object when the arguments were JSON, else text.
    arguments: Value,
}

/// A recorded llm step inside a live replay.
#[derive(Clone, Debug)]
struct LlmStep {
    step: usize,
    model: Option<String>,
    response_id: Option<String>,
    start_ns: u64,
    expected_digest: Option<String>,
    choices: Vec<Choice>,
    tokens: tape::StepTokens,
    /// Whether an incoming request must match the recording to be served:
    /// strict mode, up to and including a fork's first edited step.
    strict: bool,
    /// Whether the answer is a fork's edit rather than the recording's.
    edited: bool,
    /// Where the step was recorded — its span's tenant, trace and span — so
    /// a divergence can be explained by re-reading the recorded request
    /// rather than holding every request for the replay's life.
    origin: (String, String, String),
}

/// A recorded tool step inside a live replay.
#[derive(Clone, Debug)]
struct ToolStep {
    step: usize,
    tool: Option<String>,
    arguments: Value,
    result: Value,
    served: bool,
    /// Why the arguments or result could not be snapshotted, when they could
    /// not; such a step is refused rather than served.
    unreadable: Option<String>,
    /// Whether the result is a fork's edit rather than the recording's.
    edited: bool,
}

/// One served or refused call, for the status document.
#[derive(Clone, Debug)]
struct CallRecord {
    step: usize,
    matched: bool,
    at_ns: u64,
}

/// The first divergence a replay saw.
#[derive(Clone, Debug)]
struct Divergence {
    step: usize,
    expected_digest: Option<String>,
    got_digest: Option<String>,
    /// Where the request first differed from the recording, once explained
    /// ([`Registry::explain_divergence`]).
    difference: Option<Value>,
}

/// A fork's state inside a live replay.
#[derive(Clone, Debug)]
struct ForkState {
    info: ForkInfo,
    /// Each edit's step, kind and whether it has been served.
    edits: Vec<(usize, &'static str, bool)>,
    /// When a call was first answered `replay_live`.
    live_at_ns: Option<u64>,
    /// Calls answered `replay_live`.
    live_calls: u64,
}

impl ForkState {
    fn served(&mut self, step: usize) {
        if let Some(edit) = self.edits.iter_mut().find(|edit| edit.0 == step) {
            edit.2 = true;
        }
    }

    /// The refusal for a call past the live point, noting it.
    fn live(&mut self, served: usize, now_ns: u64) -> Option<ReplayError> {
        let live_after = self.info.live_after?;
        self.live_at_ns.get_or_insert(now_ns);
        self.live_calls = self.live_calls.saturating_add(1);
        Some(ReplayError::Live {
            live_after,
            served,
            fork_id: Some(self.info.id.clone()),
        })
    }

    fn to_json(&self) -> Value {
        json!({
            "id": self.info.id,
            "parent": self.info.parent,
            "step": self.info.step,
            "edits": self
                .edits
                .iter()
                .map(|(step, kind, served)| json!({"step": step, "kind": kind, "served": served}))
                .collect::<Vec<_>>(),
            "live_after": self.info.live_after,
            "live_at_ns": self.live_at_ns,
            "live_calls": self.live_calls,
        })
    }
}

/// Everything an erasure could name that a replay holds a copy of: the spans
/// its served content came from, by key and by the trace, session and tenant
/// they belong to, and every payload reference their stored values carried —
/// requests included, because a digest kept for strict matching would still
/// confirm a guess at erased content.
#[derive(Debug, Default)]
struct Coverage {
    keys: HashSet<(String, String, String)>,
    traces: HashSet<(String, String)>,
    sessions: HashSet<(String, String)>,
    tenants: HashSet<String>,
    payloads: HashSet<String>,
    /// Heap the strings inside the sets hold, kept as they are inserted so
    /// the footprint is known at every step without walking the sets.
    strings: usize,
}

impl Coverage {
    /// Records one snapshotted step, from its STORED values.
    fn add(&mut self, step: &TapeStep) {
        let tenant = &step.detail.tenant;
        let key = (tenant.clone(), step.trace_id.clone(), step.span_id.clone());
        let held = key.0.heap() + key.1.heap() + key.2.heap();
        if self.keys.insert(key) {
            self.strings += held;
        }
        let trace = (tenant.clone(), step.trace_id.clone());
        let held = trace.0.heap() + trace.1.heap();
        if self.traces.insert(trace) {
            self.strings += held;
        }
        if let Some(session) = &step.detail.session {
            let session = (tenant.clone(), session.clone());
            let held = session.0.heap() + session.1.heap();
            if self.sessions.insert(session) {
                self.strings += held;
            }
        }
        if self.tenants.insert(tenant.clone()) {
            self.strings += tenant.len();
        }
        let mut references = HashSet::new();
        for value in [&step.request, &step.response].into_iter().flatten() {
            tape::collect_references(value, &mut references);
        }
        for reference in references {
            let held = reference.heap();
            if self.payloads.insert(reference) {
                self.strings += held;
            }
        }
    }

    /// What the coverage holds: each set's table, and the strings in it.
    fn heap(&self) -> usize {
        set_heap(&self.keys)
            + set_heap(&self.traces)
            + set_heap(&self.sessions)
            + set_heap(&self.tenants)
            + set_heap(&self.payloads)
            + self.strings
    }

    /// Whether `record`'s erasure covers anything this replay holds: by its
    /// subject — the same predicates the store's own read mask applies — or
    /// by any key it resolved.
    fn covers(&self, record: &EraseRecord) -> bool {
        let by_subject = match &record.subject {
            Subject::Trace { trace_id, tenant } => {
                self.traces.contains(&(tenant.clone(), trace_id.clone()))
            }
            Subject::Span {
                trace_id,
                span_id,
                tenant,
            } => self
                .keys
                .contains(&(tenant.clone(), trace_id.clone(), span_id.clone())),
            Subject::Session { session_id, tenant } => self
                .sessions
                .contains(&(tenant.clone(), session_id.clone())),
            Subject::Tenant { tenant } => self.tenants.contains(tenant),
            Subject::Payload { reference } => self.payloads.contains(reference),
        };
        by_subject || record.span_keys.iter().any(|key| self.keys.contains(key))
    }
}

struct Replay {
    run: String,
    /// The tenant whose data the replay holds: `Some` when it was created
    /// under a tenant scope, `None` for the operator view. Also the share of
    /// the registry it counts against.
    tenant: Option<String>,
    mode: Mode,
    expires_at_ns: u64,
    llm: Vec<LlmStep>,
    tools: Vec<ToolStep>,
    served: usize,
    diverged: Option<Divergence>,
    calls: Vec<CallRecord>,
    bytes: usize,
    coverage: Coverage,
    /// The newest erasure known not to cover this replay: the store's epoch
    /// read BEFORE the snapshot (an erasure at or below it was already
    /// masking that read), raised as later erasures are checked.
    erasure_epoch: u64,
    /// Set when an erasure covered the replay and it was withdrawn.
    erased: Option<u64>,
    /// Held by the one call consuming an llm step of this replay, from
    /// reading the cursor to committing it. Rendering happens outside the
    /// registry's lock, so without a turn of its own each concurrent call
    /// would render the same step and all but one throw the work away.
    serving: Arc<Mutex<()>>,
    /// Model calls refused because every recorded step had been served — an
    /// agent that asked more than the recording holds, which a tape test
    /// counts as a failure.
    exhausted_calls: u64,
    /// When the replay is a fork, its edits and live point.
    fork: Option<ForkState>,
}

impl Replay {
    /// Whether a caller bound to `binding` (or unbound, `None`) may use this
    /// replay. An unbound caller may use any; a bound one only a replay of
    /// its own tenant's data — never an operator-view replay, which may hold
    /// several tenants'.
    fn visible_to(&self, binding: Option<&str>) -> bool {
        match binding {
            None => true,
            Some(bound) => self.tenant.as_deref() == Some(bound),
        }
    }

    fn record(&mut self, step: usize, matched: bool, at_ns: u64) {
        if self.calls.len() < MAX_RECORDED_CALLS {
            self.calls.push(CallRecord {
                step,
                matched,
                at_ns,
            });
        }
    }

    /// Drops everything the replay holds, keeping only what it takes to keep
    /// answering "erased" until it expires: its tenant (for visibility) and
    /// its expiry. A withdrawn replay counts against no live bound; withdrawn
    /// replays have their own (see `forget_excess_withdrawn`).
    fn withdraw(&mut self, erasure: u64) {
        self.erased = Some(erasure);
        self.run = String::new();
        self.llm = Vec::new();
        self.tools = Vec::new();
        self.calls = Vec::new();
        self.diverged = None;
        self.coverage = Coverage::default();
        self.fork = None;
        self.bytes = 0;
    }

    fn unreadable_tool_steps(&self) -> Vec<usize> {
        self.tools
            .iter()
            .filter(|tool| tool.unreadable.is_some())
            .map(|tool| tool.step)
            .collect()
    }
}

/// Monotonic time as a registry reads it: real elapsed time in service, a
/// hand-moved reading in tests — so a test can hold a call across a replay's
/// expiry without sleeping through a TTL.
type Clock = Arc<dyn Fn() -> Duration + Send + Sync>;

/// The registry's clock in service: time elapsed since the registry was made.
fn monotonic() -> Clock {
    let origin = Instant::now();
    Arc::new(move || origin.elapsed())
}

/// A registry call's place in time: the Unix time its caller handed it — when
/// the request arrived — and the registry clock's reading then.
///
/// A call can wait after it arrives: for the registry's lock, and in
/// [`Registry::next_llm`] for its replay's turn while an earlier call renders.
/// Expiry judged by the arrival time alone would let a call that queued just
/// before a replay expired consume it after, and every call queued behind it
/// after that. So each check reads the time afresh ([`Registry::now`]): the
/// arrival advanced by the monotonic time the call has spent, which a wall
/// clock stepped backwards cannot turn back.
#[derive(Clone, Copy)]
struct CallTime {
    arrived_ns: u64,
    began: Duration,
}

/// The in-memory registry of live replays.
pub struct Registry {
    replays: Mutex<HashMap<String, Replay>>,
    limits: Limits,
    clock: Clock,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new(Limits::default())
    }
}

impl Registry {
    /// A registry with explicit bounds; [`Registry::default`] uses the
    /// documented ones. Every bound is at least one.
    pub fn new(limits: Limits) -> Self {
        Self::with_clock(limits, monotonic())
    }

    /// [`Self::new`], reading time from `clock`.
    fn with_clock(limits: Limits, clock: Clock) -> Self {
        Self {
            clock,
            replays: Mutex::new(HashMap::new()),
            limits: Limits {
                max_live: limits.max_live.max(1),
                max_bytes: limits.max_bytes.max(1),
                max_live_per_tenant: limits.max_live_per_tenant.max(1),
                max_bytes_per_tenant: limits.max_bytes_per_tenant.max(1),
                max_spans: limits.max_spans.max(1),
                max_response_bytes: limits.max_response_bytes.max(1),
                ..limits
            },
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Replay>> {
        // A panic while holding the lock leaves at worst a replay whose
        // cursor did not advance; recovering the map is strictly better than
        // refusing every replay for the life of the process.
        self.replays
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A call that arrived at `now_ns`, starting its clock.
    fn arrived(&self, now_ns: u64) -> CallTime {
        CallTime {
            arrived_ns: now_ns,
            began: (self.clock)(),
        }
    }

    /// The Unix time `call` has reached: its arrival, advanced by the time it
    /// has spent since.
    fn now(&self, call: CallTime) -> u64 {
        let spent = (self.clock)().saturating_sub(call.began);
        call.arrived_ns
            .saturating_add(u64::try_from(spent.as_nanos()).unwrap_or(u64::MAX))
    }

    /// The registry's lock, and the time `call` is judged at while holding
    /// it — read once the lock is held, so a wait for it counts.
    fn lock_at(&self, call: CallTime) -> (std::sync::MutexGuard<'_, HashMap<String, Replay>>, u64) {
        let replays = self.lock();
        let now_ns = self.now(call);
        (replays, now_ns)
    }

    /// Snapshots `run_id`'s tape under `tenant` and registers a replay of it.
    ///
    /// The run's read and every payload resolved for it share ONE compute
    /// budget ([`crate::Config::query_deadline`]). `tape_max_bytes`
    /// (`--tape-max-bytes`) bounds the payload bytes the replay RETAINS —
    /// answers and tool records; requests are resolved one at a time to be
    /// digested and are not charged. Refused, naming the step, when a model
    /// step recorded no answer (content capture was off), when an answer or —
    /// in strict mode — a request is an offloaded payload that could not be
    /// read or is past the budget, or when strict mode has no recorded
    /// message list to compare against; refused too when the run has more
    /// spans than [`Limits::max_spans`], and — as soon as building the
    /// snapshot passes it — when the replay would hold more than a whole
    /// share.
    pub fn create(
        &self,
        store: &Store,
        tenant: Option<&str>,
        run_id: &str,
        mode: Mode,
        tape_max_bytes: usize,
        now_ns: u64,
    ) -> Result<Created, ReplayError> {
        self.create_fork(
            store,
            tenant,
            run_id,
            mode,
            &ForkSpec::default(),
            tape_max_bytes,
            now_ns,
        )
    }

    /// [`Self::create`], forked as `fork` says ([`crate::fork`]): its edits
    /// are served in place of the recorded answers at their steps, strict
    /// matching applies only up to and including the first edited step, and
    /// after its live point every call is refused as [`ReplayError::Live`].
    /// Steps after the live point are not snapshotted at all, so they need
    /// not be replayable; nor need an edited step's recorded answer.
    ///
    /// Refused as [`ReplayError::Invalid`] when an edit names a step the run
    /// does not have or of another kind, when the live point is past the
    /// run's last step, or when an edited answer holds no assistant message —
    /// the last checked before the store is read.
    #[allow(clippy::too_many_arguments)]
    pub fn create_fork(
        &self,
        store: &Store,
        tenant: Option<&str>,
        run_id: &str,
        mode: Mode,
        fork: &ForkSpec,
        tape_max_bytes: usize,
        now_ns: u64,
    ) -> Result<Created, ReplayError> {
        // Edited answers decoded first: a malformed one is the caller's
        // mistake and should not cost a store read.
        let mut plan = Plan {
            edits: HashMap::new(),
            live_after: fork.live_after,
            strict_through: fork.first_edit(),
        };
        for edit in fork.edits.iter().take(MAX_EDITS) {
            let edited = match &edit.value {
                EditValue::Response(value) => {
                    Edited::Answer(edited_choices(value).map_err(|why| {
                        ReplayError::Invalid(format!("the edit at step {}: {why}", edit.step))
                    })?)
                }
                EditValue::ToolResult(value) => Edited::ToolResult(value.clone()),
            };
            plan.edits.insert(edit.step, edited);
        }
        let call = self.arrived(now_ns);
        // Cheap refusal first: a full share should not cost a store read.
        {
            let (mut replays, at_ns) = self.lock_at(call);
            self.purge(&mut replays, at_ns);
            self.ensure_room(&replays, tenant, 0, at_ns)?;
        }
        // The epoch BEFORE the read: an erasure at or below it was already
        // masking what the read saw; anything newer is checked on every call.
        let erasure_epoch = store.erasures.latest_id()?;
        let deadline = store.query_deadline(Instant::now());
        let max_spans = self.limits.max_spans;
        let read = store.run_tape(tenant, run_id, max_spans, max_spans, deadline)?;
        if read.spans.is_empty() {
            return Err(ReplayError::RunNotFound);
        }
        if read.truncated {
            return Err(ReplayError::Unreplayable(format!(
                "the run has more than {max_spans} spans, more than one replay holds; replaying \
                 its first {max_spans} would end the agent's run early with every later model \
                 call refused as if the recording ended there"
            )));
        }
        fork.check_against(&read.steps)
            .map_err(ReplayError::Invalid)?;
        let steps = read.steps.len();
        let budget = SnapshotBudget {
            tape_max_bytes,
            overhead: replay_overhead(run_id, tenant, fork),
            share_bytes: self.share_bytes(),
        };
        let snapshot = snapshot(store, tenant, deadline, read.steps, mode, budget, plan)?;

        // Room judged at the time the snapshot is done, which may be a whole
        // query budget after the request arrived; the replay's lifetime still
        // counts from its arrival, the time its creation response states.
        let (mut replays, at_ns) = self.lock_at(call);
        self.purge(&mut replays, at_ns);
        self.ensure_room(&replays, tenant, snapshot.bytes, at_ns)?;
        let id = new_id("rp_");
        let ttl_ns = self.limits.ttl.as_nanos().min(u128::from(u64::MAX)) as u64;
        let fork = fork.cut().map(|step| ForkInfo {
            id: new_id("fk_"),
            parent: run_id.to_owned(),
            step,
            edits: fork
                .edits
                .iter()
                .map(|edit| (edit.step, edit.value.kind()))
                .collect(),
            live_after: fork.live_after,
        });
        let replay = Replay {
            run: run_id.to_owned(),
            tenant: tenant.map(str::to_owned),
            mode,
            expires_at_ns: now_ns.saturating_add(ttl_ns),
            llm: snapshot.llm,
            tools: snapshot.tools,
            served: 0,
            diverged: None,
            calls: Vec::new(),
            bytes: snapshot.bytes,
            coverage: snapshot.coverage,
            erasure_epoch,
            erased: None,
            serving: Arc::new(Mutex::new(())),
            exhausted_calls: 0,
            fork: fork.clone().map(|info| ForkState {
                edits: info
                    .edits
                    .iter()
                    .map(|(step, kind)| (*step, *kind, false))
                    .collect(),
                info,
                live_at_ns: None,
                live_calls: 0,
            }),
        };
        let created = Created {
            id: id.clone(),
            run: run_id.to_owned(),
            steps,
            llm_steps: replay.llm.len(),
            mode,
            expires_at_ns: replay.expires_at_ns,
            unreadable_tool_steps: replay.unreadable_tool_steps(),
            fork,
        };
        replays.insert(id, replay);
        Ok(created)
    }

    /// Refuses a snapshot of `bytes` that the share of `tenant` (its data's
    /// tenant, or the operator view) or the registry cannot hold now.
    ///
    /// A snapshot larger than a whole share is [`ReplayError::TooLarge`]:
    /// waiting cannot make it fit, and a `Retry-After` would send a client
    /// round a loop that never ends. The share is checked before the
    /// registry, so a caller is told about its own share — and its retry time
    /// is computed from its own replays, which reveals nothing about anybody
    /// else's.
    fn ensure_room(
        &self,
        replays: &HashMap<String, Replay>,
        tenant: Option<&str>,
        bytes: usize,
        now_ns: u64,
    ) -> Result<(), ReplayError> {
        let limits = &self.limits;
        let share_bytes = self.share_bytes();
        if bytes > share_bytes {
            return Err(ReplayError::TooLarge {
                bytes,
                limit: share_bytes,
            });
        }
        let held = || {
            replays
                .values()
                .filter(|replay| replay.erased.is_none())
                .map(|replay| (replay.tenant.as_deref(), replay.expires_at_ns, replay.bytes))
        };
        let share: Vec<(u64, usize)> = held()
            .filter(|(owner, _, _)| *owner == tenant)
            .map(|(_, expires, size)| (expires, size))
            .collect();
        if let Some(retry_after_seconds) = wait_for_room(
            share,
            limits.max_live_per_tenant,
            share_bytes,
            bytes,
            now_ns,
        ) {
            return Err(ReplayError::Full {
                own_share: true,
                max_live: limits.max_live_per_tenant,
                max_bytes: share_bytes,
                retry_after_seconds,
            });
        }
        let all: Vec<(u64, usize)> = held().map(|(_, expires, size)| (expires, size)).collect();
        if let Some(retry_after_seconds) =
            wait_for_room(all, limits.max_live, limits.max_bytes, bytes, now_ns)
        {
            return Err(ReplayError::Full {
                own_share: false,
                max_live: limits.max_live,
                max_bytes: limits.max_bytes,
                retry_after_seconds,
            });
        }
        Ok(())
    }

    /// The most one share may hold, which is also the largest replay.
    fn share_bytes(&self) -> usize {
        self.limits.max_bytes_per_tenant.min(self.limits.max_bytes)
    }

    fn purge(&self, replays: &mut HashMap<String, Replay>, now_ns: u64) {
        replays.retain(|_, replay| replay.expires_at_ns > now_ns);
    }

    /// Forgets withdrawn replays past their bound, once `tenant`'s share has
    /// had one withdrawn.
    ///
    /// A withdrawn replay counts against no bound — an erasure must not lock
    /// a tenant out of replays for an hour — but it is still an entry every
    /// call's purge walks, and creating, erasing and asking again would
    /// otherwise pile them up without limit. So each share keeps as many
    /// withdrawn replays as it may hold live ones, and the registry as many
    /// as it may hold live in all; past that, the one closest to expiring —
    /// with the least time left to answer `410` anyway — is forgotten, and
    /// its id answers `404` from then on.
    fn forget_excess_withdrawn(&self, replays: &mut HashMap<String, Replay>, tenant: Option<&str>) {
        let mut forget = |share: Option<Option<&str>>, bound: usize| {
            let mut withdrawn: Vec<(u64, String)> = replays
                .iter()
                .filter(|(_, replay)| {
                    replay.erased.is_some()
                        && share.map_or(true, |owner| replay.tenant.as_deref() == owner)
                })
                .map(|(id, replay)| (replay.expires_at_ns, id.clone()))
                .collect();
            let excess = withdrawn.len().saturating_sub(bound);
            if excess == 0 {
                return;
            }
            withdrawn.sort_unstable();
            for (_, id) in withdrawn.into_iter().take(excess) {
                replays.remove(&id);
            }
        };
        forget(Some(tenant), self.limits.max_live_per_tenant);
        forget(None, self.limits.max_live);
    }

    /// The live replay `id` as `binding` may see it, or why not — after
    /// checking it against every erasure recorded since it was last checked.
    ///
    /// `latest_erasure` is the store's erasure epoch, read by the caller
    /// before taking the registry lock so that the common case never waits
    /// on the tombstone log's lock (held across an fsync while an erasure is
    /// recorded). An erasure recorded after that read is concurrent with this
    /// call and is caught by the next one; every call that STARTS after an
    /// erasure is recorded sees it. Consulting the log under the registry
    /// lock is an in-memory read, and nothing on the erasure path takes the
    /// registry lock, so the two cannot deadlock.
    fn live<'r>(
        &self,
        replays: &'r mut HashMap<String, Replay>,
        store: &Store,
        latest_erasure: u64,
        id: &str,
        binding: Option<&str>,
        now_ns: u64,
    ) -> Result<&'r mut Replay, ReplayError> {
        self.purge(replays, now_ns);
        let withdrawn = {
            let replay = replays
                .get_mut(id)
                .filter(|replay| replay.visible_to(binding))
                .ok_or(ReplayError::NotFound)?;
            if let Some(erasure) = replay.erased {
                return Err(ReplayError::Erased { erasure });
            }
            let mut withdrawn = None;
            if latest_erasure > replay.erasure_epoch {
                let covering = store.erasures.first_since(replay.erasure_epoch, |record| {
                    replay.coverage.covers(record)
                })?;
                match covering {
                    Some(erasure) => {
                        replay.withdraw(erasure);
                        withdrawn = Some((erasure, replay.tenant.clone()));
                    }
                    None => replay.erasure_epoch = latest_erasure,
                }
            }
            withdrawn
        };
        if let Some((erasure, tenant)) = withdrawn {
            self.forget_excess_withdrawn(replays, tenant.as_deref());
            return Err(ReplayError::Erased { erasure });
        }
        replays.get_mut(id).ok_or(ReplayError::NotFound)
    }

    /// Serves the next recorded llm step to a request whose messages
    /// normalise to `got_digest` ([`openai_request_digest`],
    /// [`anthropic_request_digest`]), as `render` renders it — typically
    /// [`ServedStep::openai`] or [`ServedStep::anthropic`].
    ///
    /// The step is consumed only once it has rendered: a rendering that
    /// fails — refused as too large ([`ReplayError::ResponseTooLarge`]), or
    /// any error `render` returns — leaves the cursor where it was, as a
    /// divergence does.
    ///
    /// Calls to one replay take turns: each holds the replay's serving lock
    /// from reading the cursor to committing it, so every consumed step is
    /// rendered exactly once, and a call that waited for its turn is
    /// compared against the step after the one that was in flight — the
    /// arrival order the module documents. Rendering happens outside the
    /// registry's lock, so a large answer holds up calls to its own replay
    /// only, never every other replay. An erasure recorded while a step
    /// renders is caught at the commit, which drops the rendering unserved.
    ///
    /// Expiry is judged afresh at every check — when the call arrives, when
    /// its turn comes and at the commit — by `now_ns` advanced by the time
    /// the call has spent. A call whose turn comes after the replay expired
    /// is refused ([`ReplayError::NotFound`]) without rendering, and a
    /// rendering that finishes after it is dropped unserved: no step is
    /// consumed past the expiry the creation response stated.
    pub fn next_llm<R>(
        &self,
        store: &Store,
        id: &str,
        binding: Option<&str>,
        got_digest: Option<String>,
        now_ns: u64,
        render: impl Fn(&ServedStep) -> Result<R, ReplayError>,
    ) -> Result<R, ReplayError> {
        let call = self.arrived(now_ns);
        let serving = {
            let latest_erasure = store.erasures.latest_id()?;
            let (mut replays, now_ns) = self.lock_at(call);
            let replay = self.live(&mut replays, store, latest_erasure, id, binding, now_ns)?;
            Arc::clone(&replay.serving)
        };
        // A call that panicked holding the turn never committed, so the
        // cursor it read is still the cursor: the turn is safe to take over.
        let _turn = serving
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (served, cursor, matched, edited) = {
            // Checked afresh: the replay may have been withdrawn, or have
            // expired, while this call waited for its turn — and expiry is
            // judged at the time the turn came, not the time the call arrived.
            let latest_erasure = store.erasures.latest_id()?;
            let (mut replays, now_ns) = self.lock_at(call);
            let replay = self.live(&mut replays, store, latest_erasure, id, binding, now_ns)?;
            let cursor = replay.served;
            let Some(step) = replay.llm.get(cursor).cloned() else {
                // Steps after a fork's live point were never held, so running
                // off the end of what is held is reaching the live point.
                if let Some(live) = replay
                    .fork
                    .as_mut()
                    .and_then(|fork| fork.live(cursor, now_ns))
                {
                    return Err(live);
                }
                replay.exhausted_calls = replay.exhausted_calls.saturating_add(1);
                return Err(ReplayError::Exhausted { served: cursor });
            };
            let matched = step.expected_digest.is_some() && step.expected_digest == got_digest;
            if step.strict && !matched {
                replay.record(step.step, false, now_ns);
                if replay.diverged.is_none() {
                    replay.diverged = Some(Divergence {
                        step: step.step,
                        expected_digest: step.expected_digest.clone(),
                        got_digest: got_digest.clone(),
                        difference: None,
                    });
                }
                return Err(ReplayError::Diverged {
                    step: step.step,
                    expected_digest: step.expected_digest,
                    got_digest,
                });
            }
            let served = ServedStep {
                replay: id.to_owned(),
                step: step.step,
                model: step.model,
                response_id: step.response_id,
                start_ns: step.start_ns,
                choices: step.choices,
                tokens: step.tokens,
                max_response_bytes: self.limits.max_response_bytes,
            };
            (served, cursor, matched, step.edited)
        };
        let rendered = render(&served)?;
        // An erasure recorded while the step rendered withdraws the replay
        // here, and an expiry passed while it rendered drops it; either way
        // the rendering is dropped unserved.
        let latest_erasure = store.erasures.latest_id()?;
        let (mut replays, now_ns) = self.lock_at(call);
        let replay = self.live(&mut replays, store, latest_erasure, id, binding, now_ns)?;
        debug_assert_eq!(
            replay.served, cursor,
            "only the call holding a replay's turn moves its cursor"
        );
        replay.record(served.step, matched, now_ns);
        replay.served += 1;
        if edited {
            if let Some(fork) = replay.fork.as_mut() {
                fork.served(served.step);
            }
        }
        Ok(rendered)
    }

    /// Serves the next unserved recorded tool step, or the next one named
    /// `tool`. Tool steps have their own cursor, independent of the llm one,
    /// so a harness may take results whenever its agent calls a tool. A step
    /// whose value could not be snapshotted is refused, not consumed: the
    /// recording does not have its result, and moving past it would hand the
    /// harness the NEXT step's result as if it were this one's.
    pub fn next_tool(
        &self,
        store: &Store,
        id: &str,
        binding: Option<&str>,
        tool: Option<&str>,
        now_ns: u64,
    ) -> Result<ServedTool, ReplayError> {
        let call = self.arrived(now_ns);
        let latest_erasure = store.erasures.latest_id()?;
        let (mut replays, now_ns) = self.lock_at(call);
        let replay = self.live(&mut replays, store, latest_erasure, id, binding, now_ns)?;
        let next = replay.tools.iter_mut().find(|candidate| {
            !candidate.served && tool.map_or(true, |name| candidate.tool.as_deref() == Some(name))
        });
        let Some(next) = next else {
            // A fork holds no tool step after its live point: none left to
            // serve means the harness runs the real tool from here.
            let served = replay.served;
            if let Some(live) = replay
                .fork
                .as_mut()
                .and_then(|fork| fork.live(served, now_ns))
            {
                return Err(live);
            }
            return Err(ReplayError::ToolsExhausted {
                tool: tool.map(str::to_owned),
            });
        };
        if let Some(reason) = &next.unreadable {
            return Err(ReplayError::ToolUnreadable {
                step: next.step,
                tool: next.tool.clone(),
                reason: reason.clone(),
            });
        }
        next.served = true;
        let served = ServedTool {
            step: next.step,
            tool: next.tool.clone(),
            arguments: next.arguments.clone(),
            result: next.result.clone(),
            edited: next.edited,
        };
        if served.edited {
            if let Some(fork) = replay.fork.as_mut() {
                fork.served(served.step);
            }
        }
        Ok(served)
    }

    /// Where a request refused as a divergence at tape step `step` first
    /// differs from the recorded request, or `None` when that cannot be said
    /// — the recorded request is gone (erased), unreadable, not a message
    /// list, or the read does not fit the query budget.
    ///
    /// A replay holds only each recorded request's digest, so the recorded
    /// request is read again here, from the step's own span, under the
    /// span's tenant and the configured query budget, and outside the
    /// registry's lock. Both sides are normalised exactly as strict matching
    /// normalises them and compared message by message; the answer names the
    /// first message that differs, its role, the first field that differs
    /// (`role`, `content`, `tool_call_id`, `tool_calls`; or `missing` /
    /// `extra` when one side has fewer messages), the character where it
    /// differs, and a quote of each side from just before that character:
    ///
    /// `{"message": 1, "role": "user", "field": "content", "at_char": 22,
    ///   "recorded": "What's the weather in Paris?", "got": "What's the
    ///   weather in Lyon?", "recorded_messages": 2, "got_messages": 2}`
    ///
    /// When `step` is the replay's first divergence and this request is the
    /// one that caused it, the difference is kept with it, so the status
    /// document shows it too.
    #[allow(clippy::too_many_arguments)]
    pub fn explain_divergence(
        &self,
        store: &Store,
        id: &str,
        binding: Option<&str>,
        step: usize,
        request: &Value,
        provider: Provider,
        now_ns: u64,
    ) -> Option<Value> {
        let call = self.arrived(now_ns);
        let origin = {
            let latest_erasure = store.erasures.latest_id().ok()?;
            let (mut replays, now_ns) = self.lock_at(call);
            let replay = self
                .live(&mut replays, store, latest_erasure, id, binding, now_ns)
                .ok()?;
            replay
                .llm
                .iter()
                .find(|held| held.step == step)?
                .origin
                .clone()
        };
        let difference = first_difference(store, &origin, request, provider).ok()??;
        let got_digest = provider.request_digest(request).ok().flatten();
        let latest_erasure = store.erasures.latest_id().ok()?;
        let (mut replays, now_ns) = self.lock_at(call);
        let replay = self
            .live(&mut replays, store, latest_erasure, id, binding, now_ns)
            .ok()?;
        if let Some(divergence) = replay.diverged.as_mut() {
            if divergence.step == step
                && divergence.difference.is_none()
                && divergence.got_digest == got_digest
            {
                divergence.difference = Some(difference.clone());
            }
        }
        Some(difference)
    }

    /// The status document of `GET /v1/replays/{id}`.
    pub fn status(
        &self,
        store: &Store,
        id: &str,
        binding: Option<&str>,
        now_ns: u64,
    ) -> Result<Value, ReplayError> {
        let call = self.arrived(now_ns);
        let latest_erasure = store.erasures.latest_id()?;
        let (mut replays, now_ns) = self.lock_at(call);
        let replay = self.live(&mut replays, store, latest_erasure, id, binding, now_ns)?;
        let calls: Vec<Value> = replay
            .calls
            .iter()
            .map(|call| json!({"step": call.step, "matched": call.matched, "at_ns": call.at_ns}))
            .collect();
        let mut status = json!({
            "id": id,
            "run": replay.run,
            "mode": replay.mode.as_str(),
            "served": replay.served,
            "remaining": replay.llm.len() - replay.served,
            "diverged": replay.diverged.as_ref().map(|divergence| {
                let mut diverged = json!({
                    "step": divergence.step,
                    "expected_digest": divergence.expected_digest,
                    "got_digest": divergence.got_digest,
                });
                if let Some(difference) = &divergence.difference {
                    diverged["difference"] = difference.clone();
                }
                diverged
            }),
            "calls": calls,
            "exhausted_calls": replay.exhausted_calls,
            "unreadable_tool_steps": replay.unreadable_tool_steps(),
            "expires_at_ns": replay.expires_at_ns,
        });
        if let Some(fork) = &replay.fork {
            status["fork_id"] = json!(fork.info.id);
            status["fork"] = fork.to_json();
        }
        Ok(status)
    }

    /// Live replays, withdrawn ones included, for tests and diagnostics.
    pub fn len(&self, now_ns: u64) -> usize {
        let mut replays = self.lock();
        self.purge(&mut replays, now_ns);
        replays.len()
    }

    /// Whether no replay is live.
    pub fn is_empty(&self, now_ns: u64) -> bool {
        self.len(now_ns) == 0
    }
}

/// Seconds until replays expiring in order free room for `bytes` more under
/// `max_live` and `max_bytes`, or `None` when there is room now.
///
/// Requires `bytes <= max_bytes` and `max_live >= 1`, so room always comes
/// once every held replay has expired; the answer is the earliest expiry
/// after which both bounds hold — not merely the oldest expiry, which would
/// tell a large snapshot to retry when one small replay frees too little.
fn wait_for_room(
    mut held: Vec<(u64, usize)>,
    max_live: usize,
    max_bytes: usize,
    bytes: usize,
    now_ns: u64,
) -> Option<u64> {
    let fits =
        |count: usize, total: usize| count < max_live && total.saturating_add(bytes) <= max_bytes;
    let mut count = held.len();
    let mut total: usize = held.iter().map(|(_, size)| size).sum();
    if fits(count, total) {
        return None;
    }
    held.sort_unstable();
    let mut at = now_ns;
    for (expires, size) in held {
        count -= 1;
        total -= size;
        at = expires;
        if fits(count, total) {
            break;
        }
    }
    // Rounded up: a retry that arrives a fraction early would be refused
    // again for nothing.
    Some(at.saturating_sub(now_ns).div_ceil(1_000_000_000).max(1))
}

/// A replay's content before it is registered.
#[derive(Default)]
struct Snapshot {
    llm: Vec<LlmStep>,
    tools: Vec<ToolStep>,
    coverage: Coverage,
    /// What the replay will hold in all: the snapshot's footprint plus the
    /// replay's own overhead ([`replay_overhead`]).
    bytes: usize,
}

impl Snapshot {
    /// The footprint so far, from `steps_heap` (the steps' own heap, summed
    /// as they were added) and the collections' current capacities — so it
    /// costs nothing to ask after every step.
    fn footprint(&self, steps_heap: usize) -> usize {
        self.llm.capacity() * std::mem::size_of::<LlmStep>()
            + self.tools.capacity() * std::mem::size_of::<ToolStep>()
            + steps_heap
            + self.coverage.heap()
    }
}

/// What a snapshot may spend.
struct SnapshotBudget {
    /// `--tape-max-bytes`: payload bytes the replay may resolve and retain.
    tape_max_bytes: usize,
    /// What the replay will hold besides its snapshot ([`replay_overhead`]).
    overhead: usize,
    /// A whole share: a snapshot past it can never be held, so building it
    /// stops there.
    share_bytes: usize,
}

/// A fork's edit, decoded for serving.
enum Edited {
    /// The answer served at an llm step.
    Answer(Vec<Choice>),
    /// The result served at a tool step.
    ToolResult(Value),
}

/// How a snapshot is forked: what to serve in place of the recording, where
/// the recording stops being served, and how far strict matching applies.
#[derive(Default)]
struct Plan {
    edits: HashMap<usize, Edited>,
    live_after: Option<usize>,
    /// The first edited step; strict matching applies up to and including
    /// it. `None` for no edit: then it applies to every held step.
    strict_through: Option<usize>,
}

/// What a recorded request came to, for strict matching.
enum RecordedRequest {
    /// The step recorded no request.
    Missing,
    /// It references a payload that could not be read.
    Unreadable,
    /// It is not a message list: a bare prompt (text, or a JSON object, with
    /// or without system instructions before it), or a list holding
    /// something other than messages.
    BarePrompt,
    /// Its normalised digest.
    Digest(String),
}

/// Builds a replay's llm and tool snapshots from a run's steps as stored,
/// resolving what the replay will hold and refusing what cannot be replayed —
/// including, the moment its footprint passes a whole share, a snapshot that
/// could never be held.
fn snapshot(
    store: &Store,
    tenant: Option<&str>,
    deadline: Option<Deadline>,
    steps: Vec<TapeStep>,
    mode: Mode,
    budget: SnapshotBudget,
    mut plan: Plan,
) -> Result<Snapshot, ReplayError> {
    let mut resolver = Resolver::new(store, tenant, deadline);
    // Only what the replay RETAINS is charged: answers and tool records.
    let mut retained = ResolveBudget::new(budget.tape_max_bytes);
    let mut out = Snapshot::default();
    let mut steps_heap = 0;
    for mut step in steps {
        if !matches!(step.kind, StepKind::Llm | StepKind::Tool) {
            continue;
        }
        // Past a fork's live point nothing is served, so nothing is held —
        // and nothing there needs to be replayable. Steps come in tape
        // order, so the first one past it ends the snapshot.
        if plan.live_after.is_some_and(|cut| step.index > cut) {
            break;
        }
        let edit = plan.edits.remove(&step.index);
        let strict = mode == Mode::Strict
            && plan
                .strict_through
                .map_or(true, |through| step.index <= through);
        out.coverage.add(&step);
        if step.kind == StepKind::Llm {
            let edited = match edit {
                Some(Edited::Answer(choices)) => Some(choices),
                // A tool result edit at an llm step was refused when the
                // spec was checked against the tape.
                Some(Edited::ToolResult(_)) | None => None,
            };
            let answer = match step.response.as_mut() {
                // An edited step's recorded answer is never served, so it is
                // never resolved — nor charged, nor required to be readable.
                _ if edited.is_some() => Leftover::default(),
                // A text-bearing answer is held as the text it was written
                // as — inline, or its payload's raw bytes — because the
                // value rule's parse is lossy, and text may be served as is.
                Some(response)
                    if matches!(
                        step.detail.response_source,
                        ResponseSource::Text | ResponseSource::Body
                    ) =>
                {
                    match step.detail.response_text.take() {
                        Some(text) => {
                            *response = Value::String(text);
                            Leftover::default()
                        }
                        None => resolver.resolve_text(response, &mut retained)?,
                    }
                }
                Some(response) => resolver.resolve(response, &mut retained)?,
                None => Leftover::default(),
            };
            // Resolved on its own, digested, dropped: never retained, so
            // never charged. Its size is bounded by what ingest accepted for
            // one span, and its cost by the request's deadline.
            let request = if step.request.is_none() {
                RecordedRequest::Missing
            } else {
                let leftover =
                    resolver.resolve_request(&mut step, &mut ResolveBudget::new(usize::MAX))?;
                let request = step.request.take().unwrap_or(Value::Null);
                if leftover.any() {
                    RecordedRequest::Unreadable
                } else if step.detail.bare_prompt {
                    // Combined with system instructions a bare prompt sits
                    // in a list, and its object may even read as a message;
                    // it was still never sent as messages, so there is no
                    // message list to compare a request against.
                    RecordedRequest::BarePrompt
                } else {
                    recorded_digest(&request)
                        .map_or(RecordedRequest::BarePrompt, RecordedRequest::Digest)
                }
            };
            let llm = llm_snapshot(&step, strict, edited, answer, request, &retained)?;
            steps_heap += llm.heap();
            out.llm.push(llm);
        } else {
            let edited = match edit {
                Some(Edited::ToolResult(result)) => Some(result),
                Some(Edited::Answer(_)) | None => None,
            };
            let mut leftover = Leftover::default();
            if let Some(arguments) = step.request.as_mut() {
                leftover.merge(resolver.resolve(arguments, &mut retained)?);
            }
            // An edited result replaces the recorded one, which is then
            // never read.
            if edited.is_none() {
                if let Some(response) = step.response.as_mut() {
                    leftover.merge(resolver.resolve(response, &mut retained)?);
                }
            }
            // A result recorded as empty is a result; one never recorded —
            // content capture was off — has nothing to serve, and serving
            // `null` in its place would hand the harness an invented result.
            let is_edited = edited.is_some();
            let result = match edited {
                Some(result) => Some(result),
                None => step.response.take().or(step.detail.empty_response.take()),
            };
            let unreadable = match (leftover.unreadable, leftover.past_budget, &result) {
                (true, _, _) => Some(
                    "its recorded arguments or result is an offloaded payload that could not \
                     be read — erased, or swept by retention"
                        .to_owned(),
                ),
                (false, true, _) => Some(format!(
                    "its recorded arguments or result is an offloaded payload past \
                     --tape-max-bytes ({} of answers and tool records were resolved before \
                     it); raise --tape-max-bytes to replay it",
                    size(retained.spent())
                )),
                (false, false, None) => Some(
                    "it recorded no result — content capture was off when the run was \
                     recorded (for OpenLLMetry, TRACELOOP_TRACE_CONTENT), so there is no \
                     recorded result to serve"
                        .to_owned(),
                ),
                (false, false, Some(_)) => None,
            };
            let tool = ToolStep {
                step: step.index,
                tool: step.tool.take(),
                arguments: step.request.take().unwrap_or(Value::Null),
                result: result.unwrap_or(Value::Null),
                served: false,
                unreadable,
                edited: is_edited,
            };
            steps_heap += tool.heap();
            out.tools.push(tool);
        }
        let bytes = budget.overhead + out.footprint(steps_heap);
        if bytes > budget.share_bytes {
            return Err(ReplayError::TooLarge {
                bytes,
                limit: budget.share_bytes,
            });
        }
    }
    // Growth left spare capacity behind; a snapshot is never added to again.
    out.llm.shrink_to_fit();
    out.tools.shrink_to_fit();
    out.bytes = budget.overhead + out.footprint(steps_heap);
    Ok(out)
}

/// What a registered replay holds besides its snapshot: its registry entry
/// and id, the run id it was asked for, its tenant, the divergence it may
/// record and its call log — at its full bound, charged up front so that a
/// replay's growth after admission can never take its share or the registry
/// past their budgets.
fn replay_overhead(run: &str, tenant: Option<&str>, fork: &ForkSpec) -> usize {
    // A hash map's slots hold entries inline and run up to half empty after
    // the table grows.
    let mut bytes = 2 * std::mem::size_of::<(String, Replay)>()
        + "rp_".len()
        + 32
        + run.len()
        + tenant.map_or(0, str::len)
        // The divergence's two digests, 64 hex characters each, and the
        // explanation of where it differed, at its bound.
        + 2 * 64
        + DIFFERENCE_BYTES
        + MAX_RECORDED_CALLS * std::mem::size_of::<CallRecord>()
        // The serving lock's shared allocation: two reference counts and
        // the mutex.
        + 2 * std::mem::size_of::<usize>()
        + std::mem::size_of::<Mutex<()>>();
    if fork.is_fork() {
        // Its id twice (the creation's and the state's copy), its parent's
        // run id, and its edits' bookkeeping twice over.
        bytes += 2 * ("fk_".len() + 32)
            + run.len()
            + 2 * fork.edits.len() * std::mem::size_of::<(usize, &'static str, bool)>();
    }
    bytes
}

fn llm_snapshot(
    step: &TapeStep,
    strict: bool,
    edited: Option<Vec<Choice>>,
    answer: Leftover,
    request: RecordedRequest,
    retained: &ResolveBudget,
) -> Result<LlmStep, ReplayError> {
    let label = || format!("step {} ({})", step.index, step.detail.signature);
    let is_edited = edited.is_some();
    let choices = match edited {
        Some(choices) => choices,
        None => recorded_answer(step, answer, retained, &label)?,
    };
    let (expected_digest, why) = match request {
        RecordedRequest::Digest(digest) => (Some(digest), ""),
        RecordedRequest::Missing => (None, "recorded no request"),
        RecordedRequest::Unreadable => (
            None,
            "has a request that is an offloaded payload that could not be read — erased, or \
             swept by retention",
        ),
        RecordedRequest::BarePrompt => (
            None,
            "recorded its request as a bare prompt (text or a JSON object), or as a list of \
             something other than messages, rather than as a message list",
        ),
    };
    if strict && expected_digest.is_none() {
        return Err(ReplayError::Unreplayable(format!(
            "{} {why}, so strict mode has nothing to compare an incoming request against; \
             create the replay with \"mode\": \"loose\" to serve steps in order",
            label()
        )));
    }
    // Identifiers a stream repeats in every chunk are kept only at a length
    // a provider could have produced (see MAX_IDENTIFIER_BYTES).
    let identifier = |value: &Option<String>| {
        value
            .clone()
            .filter(|value| value.len() <= MAX_IDENTIFIER_BYTES)
    };
    Ok(LlmStep {
        step: step.index,
        model: identifier(&step.model),
        // An edited answer is not the provider's, so it carries neither the
        // recorded answer's id nor its usage: the id is invented and the
        // usage reads zero.
        response_id: match is_edited {
            true => None,
            false => identifier(&step.detail.response_id),
        },
        start_ns: step.start_ns,
        expected_digest,
        choices,
        tokens: match is_edited {
            true => tape::StepTokens::default(),
            false => step.tokens.clone(),
        },
        strict,
        edited: is_edited,
        origin: (
            step.detail.tenant.clone(),
            step.trace_id.clone(),
            step.span_id.clone(),
        ),
    })
}

/// A held llm step's recorded answer as choices, or why it cannot be
/// replayed.
fn recorded_answer(
    step: &TapeStep,
    answer: Leftover,
    retained: &ResolveBudget,
    label: &dyn Fn() -> String,
) -> Result<Vec<Choice>, ReplayError> {
    let Some(response) = &step.response else {
        return Err(ReplayError::Unreplayable(format!(
            "{} recorded no response — content capture was off when the run was recorded \
             (for OpenLLMetry, TRACELOOP_TRACE_CONTENT), so there is no answer to replay",
            label()
        )));
    };
    // Unreadable first: raising the budget cannot bring back erased bytes.
    if answer.unreadable {
        return Err(ReplayError::Unreplayable(format!(
            "{}'s response is an offloaded payload that could not be read — erased, or swept \
             by retention",
            label()
        )));
    }
    if answer.past_budget {
        return Err(ReplayError::Unreplayable(format!(
            "{}'s response is an offloaded payload past --tape-max-bytes: the answers and tool \
             records before it already resolve to {}; raise --tape-max-bytes to replay this run",
            label(),
            size(retained.spent())
        )));
    }
    let choices = recorded_choices(
        response,
        step.detail.response_source,
        &step.detail.finish_reasons,
    );
    if choices.is_empty() {
        return Err(ReplayError::Unreplayable(format!(
            "{}'s response holds no assistant message to replay",
            label()
        )));
    }
    Ok(choices)
}

/// A fork's edited answer as choices: answer text, one assistant message in
/// any shape a recording may hold (OpenAI's, Anthropic's content blocks, the
/// normalised one, OTel `parts`), a list of them, or an OpenAI response body
/// with `choices`. Refused when it holds no text and no tool call — a model
/// answer that says nothing cannot be served as one.
fn edited_choices(value: &Value) -> Result<Vec<Choice>, String> {
    let choices = match value {
        Value::String(text) => vec![text_choice(text.clone())],
        other => decoded(other, ResponseSource::Body).ok_or(
            "a \"response\" edit is answer text, an assistant message ({\"content\", \
             \"tool_calls\"} or content blocks), a list of messages, or a response body \
             with \"choices\"",
        )?,
    };
    if choices.is_empty()
        || choices
            .iter()
            .all(|choice| choice.text.is_none() && choice.calls.is_empty())
    {
        return Err("a \"response\" edit must hold answer text or a tool call".to_owned());
    }
    Ok(choices)
}

// ---------------------------------------------------------------- footprint

/// What a value a replay retains holds on the heap, counted generously: its
/// allocations and the collection overhead around them. A value's inline
/// size is charged by whatever holds it — the vector or table it sits in.
trait Footprint {
    fn heap(&self) -> usize;
}

impl Footprint for String {
    fn heap(&self) -> usize {
        self.capacity()
    }
}

impl<T: Footprint> Footprint for Option<T> {
    fn heap(&self) -> usize {
        self.as_ref().map_or(0, Footprint::heap)
    }
}

impl<T: Footprint> Footprint for Vec<T> {
    fn heap(&self) -> usize {
        self.capacity() * std::mem::size_of::<T>() + self.iter().map(Footprint::heap).sum::<usize>()
    }
}

impl Footprint for Value {
    fn heap(&self) -> usize {
        match self {
            Value::Null | Value::Bool(_) | Value::Number(_) => 0,
            Value::String(text) => text.heap(),
            Value::Array(items) => items.heap(),
            Value::Object(fields) => {
                // A B-tree allocates a whole node however few entries it
                // holds, and its nodes may run half empty after splitting.
                const ENTRY: usize = std::mem::size_of::<String>() + std::mem::size_of::<Value>();
                let nodes = match fields.len() {
                    0 => 0,
                    entries => (12 + 2 * entries) * ENTRY,
                };
                nodes
                    + fields
                        .iter()
                        .map(|(key, value)| key.heap() + value.heap())
                        .sum::<usize>()
            }
        }
    }
}

impl Footprint for Call {
    fn heap(&self) -> usize {
        self.id.heap() + self.name.heap() + self.arguments.heap()
    }
}

impl Footprint for Choice {
    fn heap(&self) -> usize {
        self.text.heap() + self.calls.heap() + self.finish_reason.heap()
    }
}

impl Footprint for LlmStep {
    fn heap(&self) -> usize {
        self.model.heap()
            + self.response_id.heap()
            + self.expected_digest.heap()
            + self.choices.heap()
            + self.origin.0.heap()
            + self.origin.1.heap()
            + self.origin.2.heap()
    }
}

impl Footprint for ToolStep {
    fn heap(&self) -> usize {
        self.tool.heap() + self.arguments.heap() + self.result.heap() + self.unreadable.heap()
    }
}

/// A hash set's table: a slot and a control byte for every bucket (a power
/// of two, kept at most seven-eighths full), plus one group of trailing
/// control bytes. The elements' own heap is not included.
fn set_heap<T>(set: &HashSet<T>) -> usize {
    match set.capacity() {
        0 => 0,
        capacity => {
            let buckets = (capacity * 8).div_ceil(7).next_power_of_two();
            buckets * (std::mem::size_of::<T>() + 1) + 16
        }
    }
}

/// A fresh id: `prefix` (`rp_` for a replay, `fk_` for a fork) and 32 hex
/// characters.
///
/// Drawn from the standard library's per-process hash keys, which are seeded
/// from the operating system's randomness, over a counter and the clock. Ids
/// are hard to guess, but they are not credentials: every call through a
/// replay still authenticates, and its tenant binding is checked.
fn new_id(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let state = std::collections::hash_map::RandomState::new();
    let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut first = state.build_hasher();
    first.write_u64(serial);
    first.write_u128(nanos);
    let high = first.finish();
    let mut second = state.build_hasher();
    second.write_u64(high);
    second.write_u64(serial.wrapping_add(0x9e37_79b9_7f4a_7c15));
    let low = second.finish();
    format!("{prefix}{high:016x}{low:016x}")
}

// ------------------------------------------------------------ normalisation

/// One message in the normalised shape: the fields step facts reassemble from
/// an indexed layout.
#[derive(Clone, Debug, Default)]
struct Normalised {
    role: Option<Value>,
    content: Option<Value>,
    tool_call_id: Option<Value>,
    tool_calls: Vec<[Option<Value>; 3]>,
}

impl Normalised {
    /// Whether the message would write no attribute at all — and so digest
    /// as if it were not there.
    fn is_empty(&self) -> bool {
        [&self.role, &self.content, &self.tool_call_id]
            .into_iter()
            .chain(self.tool_calls.iter().flatten())
            .all(|field| !field.as_ref().is_some_and(present))
    }
}

/// The digest of `messages` as a recorded request with these messages would
/// carry it: written as OpenLLMetry's indexed `gen_ai.prompt.{i}.*`
/// attributes and read back through [`semconv::step_facts`], so the value
/// rules (a JSON-object string read as JSON, nulls and empty strings absent)
/// are the store's own rather than a copy of them. `None` when there is no
/// message to digest.
fn normalised_digest(messages: Vec<Normalised>) -> Option<String> {
    let mut attributes = Map::new();
    attributes.insert("gen_ai.operation.name".to_owned(), json!("chat"));
    for (index, message) in messages.into_iter().enumerate() {
        let prefix = format!("{PROMPT_PREFIX}{index}.");
        for (field, value) in [
            ("role", message.role),
            ("content", message.content),
            ("tool_call_id", message.tool_call_id),
        ] {
            if let Some(value) = value.filter(present) {
                attributes.insert(format!("{prefix}{field}"), value);
            }
        }
        for (call, [id, name, arguments]) in message.tool_calls.into_iter().enumerate() {
            for (field, value) in [("id", id), ("name", name), ("arguments", arguments)] {
                if let Some(value) = value.filter(present) {
                    attributes.insert(format!("{prefix}tool_calls.{call}.{field}"), value);
                }
            }
        }
    }
    let span = Span {
        trace_id: "replay".to_owned(),
        span_id: "normalised".to_owned(),
        tenant: String::new(),
        parent_span_id: None,
        name: "normalised".to_owned(),
        start_time_ns: 0,
        end_time_ns: 0,
        status: String::new(),
        service: String::new(),
        attributes,
        events: Vec::new(),
        links: Vec::new(),
        extra: Map::new(),
    };
    semconv::step_facts(&span).request_digest
}

/// What OpenLLMetry records: a value that is absent, `null`, an empty string
/// or an empty list is not recorded at all.
fn present(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        _ => true,
    }
}

fn non_empty(value: Option<&Value>) -> Option<Value> {
    value.filter(|value| present(value)).cloned()
}

/// A tool call in any shape a request or recording carries: the normalised
/// `{id, name, arguments}`, or OpenAI's `{id, type, function: {name,
/// arguments}}`.
fn normalised_call(call: &Value) -> [Option<Value>; 3] {
    let function = call.get("function");
    [
        non_empty(call.get("id")),
        non_empty(
            call.get("name")
                .or_else(|| function.and_then(|f| f.get("name"))),
        ),
        non_empty(
            call.get("arguments")
                .or_else(|| function.and_then(|f| f.get("arguments"))),
        ),
    ]
}

/// One message of an OpenAI chat-completions request, or of a recording in
/// the normalised or raw-provider shape — all the same fields.
fn normalised_message(message: &Value) -> Normalised {
    let calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| calls.iter().map(normalised_call).collect())
        .unwrap_or_default();
    Normalised {
        role: non_empty(message.get("role")),
        content: non_empty(message.get("content")),
        tool_call_id: non_empty(message.get("tool_call_id")),
        tool_calls: calls,
    }
}

/// One OTel GenAI `parts` message (`gen_ai.input.messages`, and the system
/// message step facts build from `gen_ai.system_instructions`), folded into
/// the normalised shape: text parts concatenate into `content`, `tool_call`
/// parts become tool calls, and a `tool_call_response` part becomes the tool
/// result its id answers.
fn parts_message(message: &Value, parts: &[Value]) -> Normalised {
    let mut text = String::new();
    let mut has_text = false;
    let mut normalised = Normalised {
        role: non_empty(message.get("role")),
        ..Normalised::default()
    };
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(content) = part.get("content").and_then(Value::as_str) {
                    text.push_str(content);
                    has_text = true;
                }
            }
            Some("tool_call") => normalised.tool_calls.push([
                non_empty(part.get("id")),
                non_empty(part.get("name")),
                non_empty(part.get("arguments")),
            ]),
            Some("tool_call_response") => {
                normalised.tool_call_id = non_empty(part.get("id"));
                normalised.content = non_empty(part.get("response").or_else(|| part.get("result")));
            }
            _ => {}
        }
    }
    if has_text {
        normalised.content = Some(Value::String(text));
    }
    normalised
}

/// The normalised digest of a RECORDED request — `None` when it is not a
/// message list, which strict mode cannot compare.
///
/// A list holding anything but messages is not a message list. Every element
/// must be an object that says something the normalisation keeps — a role,
/// content, a tool call or a tool result: an element that normalises to
/// nothing (a JSON object prompt's own fields, keys no message has) would
/// digest exactly as if it were absent, and a request that never carried it
/// would match. Whether the list itself was sent as messages is the
/// caller's to check ([`tape::StepDetail::bare_prompt`]).
fn recorded_digest(request: &Value) -> Option<String> {
    normalised_digest(recorded_messages(request)?)
}

/// A RECORDED request's messages in the normalised shape, under the rules of
/// [`recorded_digest`] — `None` when it is not a message list.
fn recorded_messages(request: &Value) -> Option<Vec<Normalised>> {
    let messages = request.as_array()?;
    let mut normalised = Vec::with_capacity(messages.len());
    for message in messages {
        if !message.is_object() {
            return None;
        }
        let message = match message.get("parts").and_then(Value::as_array) {
            Some(parts) if message.get("content").is_none() => parts_message(message, parts),
            _ => normalised_message(message),
        };
        if message.is_empty() {
            return None;
        }
        normalised.push(message);
    }
    Some(normalised)
}

/// The normalised digest of an OpenAI chat-completions request body.
///
/// Each message contributes what OpenLLMetry records of it: `role`; `content`
/// when non-empty (a list of content parts as the list); `tool_call_id`; and
/// each tool call's `id`, `function.name` and `function.arguments`. Errs when
/// `messages` is not a list.
pub fn openai_request_digest(body: &Value) -> Result<Option<String>, String> {
    Ok(normalised_digest(openai_messages(body)?))
}

/// An OpenAI chat-completions request body's messages, normalised as
/// [`openai_request_digest`] digests them.
fn openai_messages(body: &Value) -> Result<Vec<Normalised>, String> {
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or("the request body must carry a \"messages\" array")?;
    Ok(messages.iter().map(normalised_message).collect())
}

/// The normalised digest of an Anthropic messages request body.
///
/// A `system` prompt becomes a leading `{"role": "system", "content": …}`
/// message, exactly as OpenLLMetry's Anthropic instrumentation records it,
/// followed by each message's `role` and `content` (a list of content blocks
/// as the list). Errs when `messages` is not a list.
pub fn anthropic_request_digest(body: &Value) -> Result<Option<String>, String> {
    Ok(normalised_digest(anthropic_messages(body)?))
}

/// An Anthropic messages request body's messages, normalised as
/// [`anthropic_request_digest`] digests them: a `system` prompt first.
fn anthropic_messages(body: &Value) -> Result<Vec<Normalised>, String> {
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or("the request body must carry a \"messages\" array")?;
    let mut normalised = Vec::with_capacity(messages.len() + 1);
    if let Some(system) = non_empty(body.get("system")) {
        normalised.push(Normalised {
            role: Some(json!("system")),
            content: Some(system),
            ..Normalised::default()
        });
    }
    normalised.extend(messages.iter().map(|message| Normalised {
        role: non_empty(message.get("role")),
        content: non_empty(message.get("content")),
        ..Normalised::default()
    }));
    Ok(normalised)
}

// ---------------------------------------------------- divergence explained

/// The recorded request of the step at `origin` (its span's tenant, trace
/// and span), re-read and compared with `request`; see
/// [`Registry::explain_divergence`].
fn first_difference(
    store: &Store,
    origin: &(String, String, String),
    request: &Value,
    provider: Provider,
) -> crate::Result<Option<Value>> {
    let (tenant, trace_id, span_id) = origin;
    let Ok(got) = provider.messages(request) else {
        return Ok(None);
    };
    let deadline = store.query_deadline(Instant::now());
    let run = format!("{}{trace_id}", semconv::TRACE_RUN_PREFIX);
    let read = store.run_tape(
        Some(tenant),
        &run,
        tape::MAX_TAPE_SPANS,
        tape::MAX_TAPE_SPANS,
        deadline,
    )?;
    let Some(mut recorded) = read.steps.into_iter().find(|step| step.span_id == *span_id) else {
        return Ok(None);
    };
    let leftover = Resolver::new(store, Some(tenant), deadline)
        .resolve_request(&mut recorded, &mut ResolveBudget::new(usize::MAX))?;
    if leftover.any() || recorded.detail.bare_prompt {
        return Ok(None);
    }
    let Some(expected) = recorded.request.as_ref().and_then(recorded_messages) else {
        return Ok(None);
    };
    Deadline::check(deadline, 0)?;
    Ok(describe_difference(&expected, &got))
}

/// The first place `got` differs from `expected`, as
/// [`Registry::explain_divergence`] describes it; `None` when they
/// normalise alike.
fn describe_difference(expected: &[Normalised], got: &[Normalised]) -> Option<Value> {
    let counts = |mut body: Value| {
        body["recorded_messages"] = json!(expected.len());
        body["got_messages"] = json!(got.len());
        body
    };
    for index in 0..expected.len().max(got.len()) {
        match (expected.get(index), got.get(index)) {
            (Some(recorded), Some(asked)) => {
                if normalised_digest(vec![recorded.clone()])
                    == normalised_digest(vec![asked.clone()])
                {
                    continue;
                }
                let fields = [
                    (
                        "role",
                        field_text(recorded.role.as_ref()),
                        field_text(asked.role.as_ref()),
                    ),
                    (
                        "content",
                        field_text(recorded.content.as_ref()),
                        field_text(asked.content.as_ref()),
                    ),
                    (
                        "tool_call_id",
                        field_text(recorded.tool_call_id.as_ref()),
                        field_text(asked.tool_call_id.as_ref()),
                    ),
                    (
                        "tool_calls",
                        calls_text(&recorded.tool_calls),
                        calls_text(&asked.tool_calls),
                    ),
                ];
                let (field, before, after) = fields
                    .into_iter()
                    .find(|(_, before, after)| before != after)
                    .unwrap_or(("message", String::new(), String::new()));
                let at = before
                    .chars()
                    .zip(after.chars())
                    .take_while(|(left, right)| left == right)
                    .count();
                return Some(counts(json!({
                    "message": index,
                    "role": role_text(recorded).or_else(|| role_text(asked)),
                    "field": field,
                    "at_char": at,
                    "recorded": quote_from(&before, at),
                    "got": quote_from(&after, at),
                })));
            }
            (Some(recorded), None) => {
                return Some(counts(json!({
                    "message": index,
                    "role": role_text(recorded),
                    "field": "missing",
                    "at_char": 0,
                    "recorded": quote_from(&message_text(recorded), 0),
                    "got": null,
                })))
            }
            (None, Some(asked)) => {
                return Some(counts(json!({
                    "message": index,
                    "role": role_text(asked),
                    "field": "extra",
                    "at_char": 0,
                    "recorded": null,
                    "got": quote_from(&message_text(asked), 0),
                })))
            }
            (None, None) => break,
        }
    }
    None
}

/// A normalised field as the text a reader compares: under the value rule
/// strict matching applies ([`semconv::field_value`] — JSON-shaped text is
/// read as JSON), a string as itself, anything else as compact JSON, and an
/// absent field as nothing.
fn field_text(value: Option<&Value>) -> String {
    match value.map(semconv::field_value) {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text,
        Some(other) => json_text(&other),
    }
}

/// A message's tool calls as one comparable text.
fn calls_text(calls: &[[Option<Value>; 3]]) -> String {
    if calls.is_empty() {
        return String::new();
    }
    let rendered: Vec<Value> = calls
        .iter()
        .map(|[id, name, arguments]| {
            json!({
                "id": id.as_ref().map(semconv::field_value),
                "name": name.as_ref().map(semconv::field_value),
                "arguments": arguments.as_ref().map(semconv::field_value),
            })
        })
        .collect();
    json_text(&Value::Array(rendered))
}

/// A whole message as one text, for a message only one side has.
fn message_text(message: &Normalised) -> String {
    let content = field_text(message.content.as_ref());
    if content.is_empty() {
        calls_text(&message.tool_calls)
    } else {
        content
    }
}

fn role_text(message: &Normalised) -> Option<String> {
    message
        .role
        .as_ref()
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// [`DIFFERENCE_CHARS`] characters of `text` starting a little before
/// character `at`, marked with `…` where it was cut.
fn quote_from(text: &str, at: usize) -> String {
    let start = at.saturating_sub(DIFFERENCE_CONTEXT);
    let total = text.chars().count();
    let mut quote: String = text.chars().skip(start).take(DIFFERENCE_CHARS).collect();
    if start > 0 {
        quote.insert(0, '…');
    }
    if start + DIFFERENCE_CHARS < total {
        quote.push('…');
    }
    quote
}

// --------------------------------------------------------- recorded answers

/// The recorded answer as choices, decoded by where it was recorded
/// (`source`): answer text is text whatever it looks like; a message source
/// holds the normalised message list (indexed layouts, native events) or OTel
/// `parts` messages; a provider body is a raw response object (OpenAI
/// `choices`, Anthropic `content` blocks), or text when it is none.
///
/// Provenance decides rather than the value's keys, because a structured
/// answer is free to use any key: `{"content": "hi", "count": 2}` recorded as
/// the answer's text is not a message saying "hi". A text-bearing answer
/// arrives as the string it was recorded as (see [`snapshot`]), so text is
/// served byte for byte, never rebuilt from what a JSON parse kept.
fn recorded_choices(
    response: &Value,
    source: ResponseSource,
    finish_reasons: &[String],
) -> Vec<Choice> {
    let mut choices = match (source, response) {
        (_, Value::Null) => Vec::new(),
        (ResponseSource::Text, _) => vec![text_choice(answer_text(response))],
        // A body is held as its text: read as JSON to find an envelope, and
        // served as that same text when it holds none.
        (ResponseSource::Body, Value::String(_)) => {
            decoded(&semconv::field_value(response), source)
                .unwrap_or_else(|| vec![text_choice(answer_text(response))])
        }
        _ => decoded(response, source).unwrap_or_else(|| vec![text_choice(answer_text(response))]),
    };
    for (index, choice) in choices.iter_mut().enumerate() {
        if choice.finish_reason.is_none() {
            choice.finish_reason = finish_reasons.get(index).cloned();
        }
    }
    choices
}

/// A message source's or provider body's value as choices: a message list, a
/// provider envelope (a body's OpenAI `choices`), or one message — `None`
/// when it is none of these, and so is served as text.
fn decoded(value: &Value, source: ResponseSource) -> Option<Vec<Choice>> {
    match value {
        Value::Array(items) if items.iter().all(is_message) && !items.is_empty() => Some(
            items
                .iter()
                .filter(|message| !is_reasoning(message))
                .map(message_choice)
                .collect(),
        ),
        Value::Object(object)
            if source == ResponseSource::Body && object.contains_key("choices") =>
        {
            Some(
                object
                    .get("choices")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|choice| {
                        let mut parsed =
                            message_choice(choice.get("message").unwrap_or(&Value::Null));
                        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                            parsed.finish_reason = Some(reason.to_owned());
                        }
                        parsed
                    })
                    .collect(),
            )
        }
        Value::Object(_) if is_message(value) => Some(vec![message_choice(value)]),
        _ => None,
    }
}

/// Whether a value from a message source or a provider body is shaped like a
/// message — a sanity check on those sources, never applied to answer text,
/// whose shape says nothing about what it is.
fn is_message(value: &Value) -> bool {
    value.as_object().is_some_and(|object| {
        [
            "role",
            "content",
            "parts",
            "tool_calls",
            "finish_reason",
            "stop_reason",
        ]
        .iter()
        .any(|key| object.contains_key(*key))
    })
}

/// Reasoning traces some instrumentations record as their own completion
/// message. They are not part of the answer an SDK receives.
fn is_reasoning(message: &Value) -> bool {
    matches!(
        message.get("role").and_then(Value::as_str),
        Some("thinking" | "reasoning")
    )
}

fn text_choice(text: String) -> Choice {
    Choice {
        text: Some(text),
        ..Choice::default()
    }
}

/// A structured value as JSON text: compact, keys in sorted order.
fn json_text(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

/// A recorded value served as answer text: a string as it is — answer text
/// arrives as the string it was recorded as — and anything else, a value
/// that was structured before it was ever text, as JSON text.
fn answer_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => json_text(other),
    }
}

/// Whether a list is a provider's content blocks (each an object with a
/// `type`) rather than JSON text that was parsed.
fn is_block_list(items: &[Value]) -> bool {
    !items.is_empty()
        && items
            .iter()
            .all(|item| item.get("type").and_then(Value::as_str).is_some())
}

/// One recorded message as a choice.
fn message_choice(message: &Value) -> Choice {
    let mut text = String::new();
    let mut has_text = false;
    let mut calls = Vec::new();
    let mut push_text = |piece: &str, has_text: &mut bool| {
        text.push_str(piece);
        *has_text = true;
    };
    // OTel parts and provider content blocks carry text and tool calls in one
    // list; the normalised shape carries them in two fields. Both are read.
    let blocks =
        message
            .get("parts")
            .and_then(Value::as_array)
            .or_else(|| match message.get("content") {
                Some(Value::Array(items)) if is_block_list(items) => Some(items),
                _ => None,
            });
    if let Some(blocks) = blocks {
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text" | "output_text") => {
                    if let Some(piece) = block
                        .get("text")
                        .or_else(|| block.get("content"))
                        .and_then(Value::as_str)
                    {
                        push_text(piece, &mut has_text);
                    }
                }
                Some("tool_call" | "tool_use") => {
                    if let Some(name) = block.get("name").and_then(Value::as_str) {
                        calls.push(Call {
                            id: block.get("id").and_then(Value::as_str).map(str::to_owned),
                            name: name.to_owned(),
                            arguments: block
                                .get("arguments")
                                .or_else(|| block.get("input"))
                                .cloned()
                                .unwrap_or(Value::Null),
                        });
                    }
                }
                _ => {}
            }
        }
    } else {
        match message.get("content") {
            Some(Value::String(piece)) => push_text(piece, &mut has_text),
            Some(Value::Null) | None => {}
            Some(other) => push_text(&json_text(other), &mut has_text),
        }
    }
    if let Some(list) = message.get("tool_calls").and_then(Value::as_array) {
        for call in list {
            let [id, name, arguments] = normalised_call(call);
            if let Some(Value::String(name)) = name {
                calls.push(Call {
                    id: id.and_then(|id| id.as_str().map(str::to_owned)),
                    name,
                    arguments: arguments.unwrap_or(Value::Null),
                });
            }
        }
    }
    let finish_reason = message
        .get("finish_reason")
        .or_else(|| message.get("stop_reason"))
        .and_then(Value::as_str)
        .filter(|reason| !reason.is_empty())
        .map(str::to_owned);
    Choice {
        text: has_text.then_some(text),
        calls,
        finish_reason,
    }
}

// ---------------------------------------------------------------- rendering

/// A rendered provider response: its content type and body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rendered {
    /// `application/json` or `text/event-stream`.
    pub content_type: &'static str,
    /// The body bytes.
    pub body: Vec<u8>,
}

/// A response body being rendered, refusing any write that would take it
/// past `limit` — so an oversized rendering is stopped before it is
/// buffered, not measured after.
struct Capped {
    bytes: Vec<u8>,
    limit: usize,
}

impl Capped {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }

    /// One JSON value, serialized straight into the body.
    fn json(&mut self, value: &Value) -> io::Result<()> {
        serde_json::to_writer(&mut *self, value).map_err(io::Error::from)
    }
}

impl Write for Capped {
    fn write(&mut self, piece: &[u8]) -> io::Result<usize> {
        if piece.len() > self.limit - self.bytes.len() {
            return Err(io::Error::other("past the response bound"));
        }
        self.bytes.extend_from_slice(piece);
        Ok(piece.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl ServedStep {
    /// The recorded model, else the request's — only at a length a model
    /// name could have, since a stream repeats it in every chunk and an
    /// echoed one is the caller's to choose.
    fn model_or(&self, request: &Value) -> String {
        self.model
            .clone()
            .or_else(|| {
                request
                    .get("model")
                    .and_then(Value::as_str)
                    .filter(|model| model.len() <= MAX_IDENTIFIER_BYTES)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "unknown".to_owned())
    }

    /// The finished body, or the refusal for one past the bound. Writing
    /// into memory fails only at the bound, and a `Value` always serializes,
    /// so every error is the bound.
    fn finish(
        &self,
        content_type: &'static str,
        written: io::Result<()>,
        out: Capped,
    ) -> Result<Rendered, ReplayError> {
        match written {
            Ok(()) => Ok(Rendered {
                content_type,
                body: out.bytes,
            }),
            Err(_) => Err(ReplayError::ResponseTooLarge {
                step: self.step,
                limit: out.limit,
            }),
        }
    }

    /// A stable suffix for ids this replay invents: the replay's own id
    /// characters plus the step, so two steps never share one.
    fn local_id(&self) -> String {
        let hex = self.replay.strip_prefix("rp_").unwrap_or(&self.replay);
        format!("traza{}s{}", &hex[..hex.len().min(12)], self.step)
    }

    fn created_seconds(&self) -> u64 {
        self.start_ns / 1_000_000_000
    }

    fn call_id(&self, call: &Call, prefix: &str, index: usize) -> String {
        call.id
            .clone()
            .unwrap_or_else(|| format!("{prefix}{}_{index}", self.local_id()))
    }

    /// The step as an OpenAI `ChatCompletion` — or, when the request asked to
    /// stream, as `chat.completion.chunk` server-sent events ending in
    /// `data: [DONE]`. Refused as [`ReplayError::ResponseTooLarge`] rather
    /// than rendered past [`Limits::max_response_bytes`].
    pub fn openai(&self, request: &Value) -> Result<Rendered, ReplayError> {
        let model = self.model_or(request);
        let id = self
            .response_id
            .clone()
            .unwrap_or_else(|| format!("chatcmpl-{}", self.local_id()));
        let created = self.created_seconds();
        let prompt = self.tokens.input.unwrap_or(0);
        let completion = self.tokens.output.unwrap_or(0);
        let mut usage = json!({
            "prompt_tokens": prompt,
            "completion_tokens": completion,
            "total_tokens": prompt.saturating_add(completion),
        });
        if let Some(cached) = self.tokens.cache_read {
            usage["prompt_tokens_details"] = json!({"cached_tokens": cached});
        }
        let streaming = request.get("stream").and_then(Value::as_bool) == Some(true);
        let mut out = Capped::new(self.max_response_bytes);
        if !streaming {
            let choices: Vec<Value> = self
                .choices
                .iter()
                .enumerate()
                .map(|(index, choice)| {
                    let mut message = json!({
                        "role": "assistant",
                        "content": choice.text,
                        "refusal": null,
                    });
                    if !choice.calls.is_empty() {
                        message["tool_calls"] = Value::Array(
                            choice
                                .calls
                                .iter()
                                .enumerate()
                                .map(|(position, call)| {
                                    json!({
                                        "id": self.call_id(call, "call_", position),
                                        "type": "function",
                                        "function": {
                                            "name": call.name,
                                            "arguments": openai_arguments(&call.arguments),
                                        },
                                    })
                                })
                                .collect(),
                        );
                    }
                    json!({
                        "index": index,
                        "message": message,
                        "logprobs": null,
                        "finish_reason": openai_finish(choice),
                    })
                })
                .collect();
            let body = json!({
                "id": id,
                "object": "chat.completion",
                "created": created,
                "model": model,
                "choices": choices,
                "usage": usage,
                "system_fingerprint": null,
            });
            let written = out.json(&body);
            return self.finish("application/json", written, out);
        }

        let include_usage = request
            .pointer("/stream_options/include_usage")
            .and_then(Value::as_bool)
            == Some(true);
        let chunk = |out: &mut Capped, choices: Value, usage_field: Option<Value>| {
            let mut event = json!({
                "id": id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "system_fingerprint": null,
                "choices": choices,
            });
            // With include_usage every chunk carries the field, null until the
            // final one; without it the field is absent, as the API does.
            if include_usage {
                event["usage"] = usage_field.unwrap_or(Value::Null);
            }
            out.write_all(b"data: ")?;
            out.json(&event)?;
            out.write_all(b"\n\n")
        };
        let stream = |out: &mut Capped| -> io::Result<()> {
            for (index, choice) in self.choices.iter().enumerate() {
                let delta_choice = |delta: Value, finish: Value| json!([{"index": index, "delta": delta, "logprobs": null, "finish_reason": finish}]);
                let opening = if choice.calls.is_empty() || choice.text.is_some() {
                    json!({"role": "assistant", "content": "", "refusal": null})
                } else {
                    json!({"role": "assistant", "content": null, "refusal": null})
                };
                chunk(out, delta_choice(opening, Value::Null), None)?;
                if let Some(text) = &choice.text {
                    for piece in deltas(text) {
                        chunk(
                            out,
                            delta_choice(json!({"content": piece}), Value::Null),
                            None,
                        )?;
                    }
                }
                for (position, call) in choice.calls.iter().enumerate() {
                    chunk(
                        out,
                        delta_choice(
                            json!({"tool_calls": [{
                                "index": position,
                                "id": self.call_id(call, "call_", position),
                                "type": "function",
                                "function": {"name": call.name, "arguments": ""},
                            }]}),
                            Value::Null,
                        ),
                        None,
                    )?;
                    for piece in deltas(&openai_arguments(&call.arguments)) {
                        chunk(
                            out,
                            delta_choice(
                                json!({"tool_calls": [{
                                    "index": position,
                                    "function": {"arguments": piece},
                                }]}),
                                Value::Null,
                            ),
                            None,
                        )?;
                    }
                }
                chunk(
                    out,
                    delta_choice(json!({}), json!(openai_finish(choice))),
                    None,
                )?;
            }
            if include_usage {
                chunk(out, json!([]), Some(usage.clone()))?;
            }
            out.write_all(b"data: [DONE]\n\n")
        };
        let written = stream(&mut out);
        self.finish("text/event-stream", written, out)
    }

    /// The step as an Anthropic `Message` — or, when the request asked to
    /// stream, as the `message_start` / `content_block_*` / `message_delta` /
    /// `message_stop` event sequence.
    ///
    /// Anthropic answers with one message, so a recording with several
    /// assistant messages is served as their texts concatenated and their
    /// tool calls in order. Bounded as [`Self::openai`] is.
    pub fn anthropic(&self, request: &Value) -> Result<Rendered, ReplayError> {
        let model = self.model_or(request);
        let id = self
            .response_id
            .clone()
            .unwrap_or_else(|| format!("msg_{}", self.local_id()));
        let mut text: Option<String> = None;
        let mut calls: Vec<&Call> = Vec::new();
        for choice in &self.choices {
            if let Some(piece) = &choice.text {
                text.get_or_insert_with(String::new).push_str(piece);
            }
            calls.extend(choice.calls.iter());
        }
        let finish = self
            .choices
            .iter()
            .find_map(|choice| choice.finish_reason.as_deref());
        let stop_reason = anthropic_stop(finish, !calls.is_empty());
        let mut usage = json!({
            "input_tokens": self.tokens.input.unwrap_or(0),
            "output_tokens": self.tokens.output.unwrap_or(0),
        });
        if let Some(read) = self.tokens.cache_read {
            usage["cache_read_input_tokens"] = json!(read);
        }
        if let Some(write) = self.tokens.cache_write {
            usage["cache_creation_input_tokens"] = json!(write);
        }
        let mut blocks: Vec<Value> = Vec::new();
        if let Some(text) = &text {
            blocks.push(json!({"type": "text", "text": text}));
        }
        for (position, call) in calls.iter().enumerate() {
            blocks.push(json!({
                "type": "tool_use",
                "id": self.call_id(call, "toolu_", position),
                "name": call.name,
                "input": anthropic_input(&call.arguments),
            }));
        }
        let streaming = request.get("stream").and_then(Value::as_bool) == Some(true);
        let mut out = Capped::new(self.max_response_bytes);
        if !streaming {
            let body = json!({
                "id": id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": blocks,
                "stop_reason": stop_reason,
                "stop_sequence": null,
                "usage": usage,
            });
            let written = out.json(&body);
            return self.finish("application/json", written, out);
        }

        let event = |out: &mut Capped, name: &str, data: Value| {
            out.write_all(b"event: ")?;
            out.write_all(name.as_bytes())?;
            out.write_all(b"\ndata: ")?;
            out.json(&data)?;
            out.write_all(b"\n\n")
        };
        let stream = |out: &mut Capped| -> io::Result<()> {
            let mut start_usage = usage.clone();
            start_usage["output_tokens"] = json!(0);
            event(
                out,
                "message_start",
                json!({"type": "message_start", "message": {
                    "id": id,
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": start_usage,
                }}),
            )?;
            for (index, block) in blocks.iter().enumerate() {
                let (opening, pieces, delta_type, field) = match block["type"].as_str() {
                    Some("text") => (
                        json!({"type": "text", "text": ""}),
                        deltas(block["text"].as_str().unwrap_or_default()),
                        "text_delta",
                        "text",
                    ),
                    _ => {
                        let mut opening = block.clone();
                        opening["input"] = json!({});
                        (
                            opening,
                            deltas(&json_text(&block["input"])),
                            "input_json_delta",
                            "partial_json",
                        )
                    }
                };
                event(
                    out,
                    "content_block_start",
                    json!({"type": "content_block_start", "index": index, "content_block": opening}),
                )?;
                for piece in pieces {
                    let mut delta = Map::new();
                    delta.insert("type".to_owned(), json!(delta_type));
                    delta.insert(field.to_owned(), json!(piece));
                    event(
                        out,
                        "content_block_delta",
                        json!({"type": "content_block_delta", "index": index, "delta": delta}),
                    )?;
                }
                event(
                    out,
                    "content_block_stop",
                    json!({"type": "content_block_stop", "index": index}),
                )?;
            }
            event(
                out,
                "message_delta",
                json!({"type": "message_delta",
                       "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                       "usage": {"output_tokens": self.tokens.output.unwrap_or(0)}}),
            )?;
            event(out, "message_stop", json!({"type": "message_stop"}))
        };
        let written = stream(&mut out);
        self.finish("text/event-stream", written, out)
    }
}

/// Tool-call arguments as OpenAI carries them: a JSON string.
fn openai_arguments(arguments: &Value) -> String {
    match arguments {
        Value::String(text) => text.clone(),
        Value::Null => "{}".to_owned(),
        other => json_text(other),
    }
}

/// Tool-call input as Anthropic carries it: JSON, an object when the
/// recording allows.
fn anthropic_input(arguments: &Value) -> Value {
    match arguments {
        Value::Null => json!({}),
        Value::String(text) => serde_json::from_str(text).unwrap_or_else(|_| json!(text)),
        other => other.clone(),
    }
}

/// The recorded finish reason in OpenAI's vocabulary. A recording made
/// against another provider is translated; an absent one is inferred from
/// whether the answer called tools.
fn openai_finish(choice: &Choice) -> &'static str {
    let has_calls = !choice.calls.is_empty();
    match choice
        .finish_reason
        .as_deref()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("length" | "max_tokens" | "max_output_tokens") => "length",
        Some("tool_calls" | "tool_use" | "tool_call" | "function_call") => "tool_calls",
        Some("content_filter" | "refusal" | "safety") => "content_filter",
        Some("stop" | "end_turn" | "stop_sequence") => "stop",
        _ if has_calls => "tool_calls",
        _ => "stop",
    }
}

/// The recorded finish reason in Anthropic's vocabulary, by the same rules.
fn anthropic_stop(reason: Option<&str>, has_calls: bool) -> &'static str {
    match reason.map(str::to_ascii_lowercase).as_deref() {
        Some("length" | "max_tokens" | "max_output_tokens") => "max_tokens",
        Some("tool_calls" | "tool_use" | "tool_call" | "function_call") => "tool_use",
        Some("stop_sequence") => "stop_sequence",
        Some("content_filter" | "refusal" | "safety") => "refusal",
        Some("pause_turn") => "pause_turn",
        Some("stop" | "end_turn") => "end_turn",
        _ if has_calls => "tool_use",
        _ => "end_turn",
    }
}

/// Splits stored text into stream deltas: windows of [`STREAM_DELTA_CHARS`]
/// characters (wider for long text, so an answer is at most
/// [`MAX_STREAM_DELTAS`] windows), each cut back to just after whitespace when
/// the window's second half holds some, so words usually arrive whole while
/// every piece stays at least half a window. Concatenating the pieces gives
/// the text back exactly.
fn deltas(text: &str) -> Vec<String> {
    let total = text.chars().count();
    if total == 0 {
        return Vec::new();
    }
    let window = STREAM_DELTA_CHARS.max(total.div_ceil(MAX_STREAM_DELTAS));
    let characters: Vec<char> = text.chars().collect();
    let mut pieces = Vec::new();
    let mut start = 0;
    while start < characters.len() {
        let end = (start + window).min(characters.len());
        let cut = if end == characters.len() {
            end
        } else {
            characters[start..end]
                .iter()
                .rposition(|character| character.is_whitespace())
                .filter(|position| position + 1 >= window / 2)
                .map_or(end, |position| start + position + 1)
        };
        pieces.push(characters[start..cut].iter().collect());
        start = cut;
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tape::tests::{span, TempStore};
    use crate::tape::TapeOptions;

    const NOW: u64 = 1_800_000_000_000_000_000;

    /// A `next_llm` renderer that keeps the served step itself.
    fn keep_step(step: &ServedStep) -> Result<ServedStep, ReplayError> {
        Ok(step.clone())
    }

    fn openllmetry_run(store: &Store) {
        store
            .ingest_batch(vec![
                span(
                    "t1",
                    "a-llm",
                    100,
                    json!({
                        "session.id": "run-1",
                        "gen_ai.system": "openai",
                        "llm.request.type": "chat",
                        "gen_ai.request.model": "gpt-4o",
                        "gen_ai.response.model": "gpt-4o-2024-08-06",
                        "gen_ai.response.id": "chatcmpl-rec1",
                        "gen_ai.usage.prompt_tokens": 40,
                        "gen_ai.usage.completion_tokens": 12,
                        "gen_ai.prompt.0.role": "system",
                        "gen_ai.prompt.0.content": "You are a weather bot.",
                        "gen_ai.prompt.1.role": "user",
                        "gen_ai.prompt.1.content": "Weather in Paris?",
                        "gen_ai.completion.0.role": "assistant",
                        "gen_ai.completion.0.finish_reason": "tool_calls",
                        "gen_ai.completion.0.tool_calls.0.id": "call_abc",
                        "gen_ai.completion.0.tool_calls.0.name": "get_weather",
                        "gen_ai.completion.0.tool_calls.0.arguments": "{\"city\": \"Paris\"}",
                    }),
                ),
                span(
                    "t1",
                    "b-tool",
                    200,
                    json!({
                        "session.id": "run-1",
                        "traceloop.span.kind": "tool",
                        "traceloop.entity.name": "get_weather",
                        "traceloop.entity.input": "{\"city\": \"Paris\"}",
                        "traceloop.entity.output": "rainy, 12C",
                    }),
                ),
                span(
                    "t1",
                    "c-llm",
                    300,
                    json!({
                        "session.id": "run-1",
                        "gen_ai.system": "openai",
                        "llm.request.type": "chat",
                        "gen_ai.request.model": "gpt-4o",
                        "gen_ai.usage.prompt_tokens": 70,
                        "gen_ai.usage.completion_tokens": 9,
                        "gen_ai.prompt.0.role": "system",
                        "gen_ai.prompt.0.content": "You are a weather bot.",
                        "gen_ai.prompt.1.role": "user",
                        "gen_ai.prompt.1.content": "Weather in Paris?",
                        "gen_ai.prompt.2.role": "assistant",
                        "gen_ai.prompt.2.tool_calls.0.id": "call_abc",
                        "gen_ai.prompt.2.tool_calls.0.name": "get_weather",
                        "gen_ai.prompt.2.tool_calls.0.arguments": "{\"city\": \"Paris\"}",
                        "gen_ai.prompt.3.role": "tool",
                        "gen_ai.prompt.3.tool_call_id": "call_abc",
                        "gen_ai.prompt.3.content": "rainy, 12C",
                        "gen_ai.completion.0.role": "assistant",
                        "gen_ai.completion.0.finish_reason": "stop",
                        "gen_ai.completion.0.content": "It is rainy in Paris.",
                    }),
                ),
            ])
            .expect("ingests");
    }

    fn first_request() -> Value {
        json!({"model": "gpt-4o", "messages": [
            {"role": "system", "content": "You are a weather bot."},
            {"role": "user", "content": "Weather in Paris?"},
        ]})
    }

    fn second_request() -> Value {
        json!({"model": "gpt-4o", "messages": [
            {"role": "system", "content": "You are a weather bot."},
            {"role": "user", "content": "Weather in Paris?"},
            {"role": "assistant", "content": null, "tool_calls": [{
                "id": "call_abc", "type": "function",
                // Served back compact; recorded with a space. Both parse to
                // the same JSON, which is what the value rule compares.
                "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"},
            }]},
            {"role": "tool", "tool_call_id": "call_abc", "content": "rainy, 12C"},
        ]})
    }

    #[test]
    fn an_openai_request_normalises_to_the_recorded_digest() {
        let fixture = TempStore::open("replay-digest", crate::Config::default());
        let store = fixture.store();
        openllmetry_run(store);
        let tape = store
            .tape(Some(""), "run-1", TapeOptions::default())
            .expect("reads")
            .expect("exists");
        for (request, step) in [(first_request(), 0), (second_request(), 2)] {
            let got = openai_request_digest(&request).expect("parses");
            assert_eq!(got, tape.steps[step].request_digest, "step {step}");
            // And the recorded side's own normalisation is the identity on
            // the indexed layout.
            assert_eq!(
                recorded_digest(tape.steps[step].request.as_ref().expect("request")),
                tape.steps[step].request_digest
            );
        }
        // A changed word is a different request.
        let mut changed = first_request();
        changed["messages"][1]["content"] = json!("Weather in Lyon?");
        assert_ne!(
            openai_request_digest(&changed).expect("parses"),
            tape.steps[0].request_digest
        );
        assert!(openai_request_digest(&json!({"model": "x"})).is_err());
    }

    #[test]
    fn otel_parts_and_raw_provider_messages_normalise_alike() {
        let indexed = recorded_digest(&json!([
            {"role": "system", "content": "Be brief."},
            {"role": "user", "content": "hi"},
            {"role": "assistant", "tool_calls": [{"id": "c1", "name": "f", "arguments": {"a": 1}}]},
            {"role": "tool", "tool_call_id": "c1", "content": "ok"},
        ]));
        let parts = recorded_digest(&json!([
            {"role": "system", "parts": [{"type": "text", "content": "Be brief."}]},
            {"role": "user", "parts": [{"type": "text", "content": "hi"}]},
            {"role": "assistant", "parts": [
                {"type": "tool_call", "id": "c1", "name": "f", "arguments": {"a": 1}}]},
            {"role": "tool", "parts": [{"type": "tool_call_response", "id": "c1", "response": "ok"}]},
        ]));
        let raw = recorded_digest(&json!([
            {"role": "system", "content": "Be brief."},
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{\"a\":1}"}}]},
            {"role": "tool", "tool_call_id": "c1", "content": "ok"},
        ]));
        assert!(indexed.is_some());
        assert_eq!(indexed, parts);
        assert_eq!(indexed, raw);
        assert_eq!(recorded_digest(&json!("a bare prompt")), None);
    }

    #[test]
    fn an_anthropic_system_prompt_leads_the_normalised_messages() {
        let recorded = recorded_digest(&json!([
            {"role": "system", "content": "Be brief."},
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": [{"type": "text", "text": "hello"}]},
        ]));
        let incoming = anthropic_request_digest(&json!({
            "model": "claude-x", "max_tokens": 64, "system": "Be brief.",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [{"type": "text", "text": "hello"}]},
            ],
        }))
        .expect("parses");
        assert_eq!(incoming, recorded);
    }

    #[test]
    fn strict_replays_serve_in_order_refuse_divergence_and_exhaust() {
        let fixture = TempStore::open("replay-strict", crate::Config::default());
        let store = fixture.store();
        openllmetry_run(store);
        let registry = Registry::default();
        let created = registry
            .create(store, Some(""), "run-1", Mode::Strict, 1 << 20, NOW)
            .expect("creates");
        assert_eq!((created.steps, created.llm_steps), (3, 2));
        assert!(created.id.starts_with("rp_") && created.id.len() == 35);
        assert_eq!(created.expires_at_ns, NOW + 3_600_000_000_000);

        // A divergent first call is refused and does NOT consume the step.
        let mut wrong = first_request();
        wrong["messages"][1]["content"] = json!("Weather in Rome?");
        let got = openai_request_digest(&wrong).expect("parses");
        match registry.next_llm(
            store,
            &created.id,
            Some(""),
            got.clone(),
            NOW + 1,
            keep_step,
        ) {
            Err(ReplayError::Diverged {
                step,
                expected_digest,
                got_digest,
            }) => {
                assert_eq!(step, 0);
                assert_eq!(got_digest, got);
                assert!(expected_digest.is_some() && expected_digest != got_digest);
            }
            other => panic!("expected a divergence, got {other:?}"),
        }

        let served = registry
            .next_llm(
                store,
                &created.id,
                Some(""),
                openai_request_digest(&first_request()).expect("parses"),
                NOW + 2,
                keep_step,
            )
            .expect("matches");
        assert_eq!(served.step, 0);
        let body: Value =
            serde_json::from_slice(&served.openai(&first_request()).expect("renders").body)
                .unwrap();
        assert_eq!(body["id"], json!("chatcmpl-rec1"));
        assert_eq!(body["model"], json!("gpt-4o-2024-08-06"));
        assert_eq!(body["choices"][0]["finish_reason"], json!("tool_calls"));
        let call = &body["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(call["id"], json!("call_abc"));
        assert_eq!(call["function"]["arguments"], json!("{\"city\":\"Paris\"}"));
        assert_eq!(body["usage"]["total_tokens"], json!(52));

        let served = registry
            .next_llm(
                store,
                &created.id,
                Some(""),
                openai_request_digest(&second_request()).expect("parses"),
                NOW + 3,
                keep_step,
            )
            .expect("matches");
        assert_eq!(
            served.step, 2,
            "the tool step in between is not an llm step"
        );

        match registry.next_llm(store, &created.id, Some(""), None, NOW + 4, keep_step) {
            Err(ReplayError::Exhausted { served }) => assert_eq!(served, 2),
            other => panic!("expected exhaustion, got {other:?}"),
        }

        let status = registry
            .status(store, &created.id, Some(""), NOW + 5)
            .expect("live");
        assert_eq!(status["served"], json!(2));
        assert_eq!(status["remaining"], json!(0));
        assert_eq!(status["diverged"]["step"], json!(0));
        let matched: Vec<bool> = status["calls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|call| call["matched"].as_bool().unwrap())
            .collect();
        assert_eq!(matched, [false, true, true], "exhaustion is not a call");
    }

    #[test]
    fn loose_replays_serve_regardless_and_record_matches() {
        let fixture = TempStore::open("replay-loose", crate::Config::default());
        let store = fixture.store();
        openllmetry_run(store);
        let registry = Registry::default();
        let created = registry
            .create(store, None, "run-1", Mode::Loose, 1 << 20, NOW)
            .expect("creates");
        let served = registry
            .next_llm(
                store,
                &created.id,
                None,
                Some("0".repeat(64)),
                NOW,
                keep_step,
            )
            .expect("loose serves anyway");
        assert_eq!(served.step, 0);
        let status = registry
            .status(store, &created.id, None, NOW)
            .expect("live");
        assert_eq!(status["calls"][0]["matched"], json!(false));
        assert_eq!(status["diverged"], Value::Null);
    }

    #[test]
    fn tools_next_serves_recorded_tool_results_by_name() {
        let fixture = TempStore::open("replay-tools", crate::Config::default());
        let store = fixture.store();
        openllmetry_run(store);
        let registry = Registry::default();
        let id = registry
            .create(store, Some(""), "run-1", Mode::Loose, 1 << 20, NOW)
            .expect("creates")
            .id;
        assert!(matches!(
            registry.next_tool(store, &id, Some(""), Some("search"), NOW),
            Err(ReplayError::ToolsExhausted { tool: Some(_) })
        ));
        let tool = registry
            .next_tool(store, &id, Some(""), Some("get_weather"), NOW)
            .expect("serves");
        assert_eq!(
            tool.to_json(),
            json!({"step": 1, "tool": "get_weather",
                   "arguments": {"city": "Paris"}, "result": "rainy, 12C"})
        );
        assert!(matches!(
            registry.next_tool(store, &id, Some(""), None, NOW),
            Err(ReplayError::ToolsExhausted { tool: None })
        ));
    }

    #[test]
    fn a_replay_is_visible_only_to_its_tenant_and_expires() {
        let fixture = TempStore::open("replay-tenancy", crate::Config::default());
        let store = fixture.store();
        let mut acme = span(
            "t7",
            "x",
            1,
            json!({"gen_ai.operation.name": "chat", "gen_ai.prompt.0.content": "hi",
                   "gen_ai.completion.0.content": "hello"}),
        );
        acme.tenant = "acme".into();
        store.ingest_batch(vec![acme]).expect("ingests");
        let registry = Registry::new(Limits {
            ttl: Duration::from_secs(10),
            max_live: 2,
            ..Limits::default()
        });
        let id = registry
            .create(store, Some("acme"), "trace:t7", Mode::Loose, 1 << 20, NOW)
            .expect("creates")
            .id;
        assert!(matches!(
            registry.status(store, &id, Some("beta"), NOW),
            Err(ReplayError::NotFound)
        ));
        assert!(registry.status(store, &id, Some("acme"), NOW).is_ok());
        assert!(
            registry.status(store, &id, None, NOW).is_ok(),
            "an operator sees all"
        );
        // The default tenant has no such run.
        assert!(matches!(
            registry.create(store, Some(""), "trace:t7", Mode::Loose, 1 << 20, NOW),
            Err(ReplayError::RunNotFound)
        ));
        // An operator-view replay may hold several tenants' data: no bound
        // caller may use it.
        let operator = registry
            .create(store, None, "trace:t7", Mode::Loose, 1 << 20, NOW)
            .expect("creates")
            .id;
        assert!(matches!(
            registry.status(store, &operator, Some("acme"), NOW),
            Err(ReplayError::NotFound)
        ));
        // Full at two: refused, never evicting, with a retry hint.
        match registry.create(store, None, "trace:t7", Mode::Loose, 1 << 20, NOW) {
            Err(ReplayError::Full {
                own_share,
                retry_after_seconds,
                ..
            }) => assert_eq!((own_share, retry_after_seconds), (false, 10)),
            other => panic!("expected a full registry, got {other:?}"),
        }
        let later = NOW + 10_000_000_000;
        assert!(registry.is_empty(later), "both expired at their TTL");
        assert!(matches!(
            registry.status(store, &id, None, later),
            Err(ReplayError::NotFound)
        ));
    }

    #[test]
    fn unreplayable_runs_are_refused_naming_the_step() {
        let fixture = TempStore::open("replay-refuse", crate::Config::default());
        let store = fixture.store();
        store
            .ingest_batch(vec![
                // Content capture off: usage but no prompt or completion.
                span(
                    "t2",
                    "blind",
                    1,
                    json!({"gen_ai.operation.name": "chat", "gen_ai.request.model": "m",
                           "gen_ai.usage.input_tokens": 3}),
                ),
                // A bare-text prompt: replayable loose, not strict.
                span(
                    "t3",
                    "text",
                    1,
                    json!({"gen_ai.operation.name": "chat", "llm.prompt": "hi",
                           "llm.completion": "hello"}),
                ),
            ])
            .expect("ingests");
        let registry = Registry::default();
        match registry.create(store, Some(""), "trace:t2", Mode::Loose, 1 << 20, NOW) {
            Err(ReplayError::Unreplayable(message)) => {
                assert!(
                    message.contains("step 0") && message.contains("no response"),
                    "{message}"
                )
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        match registry.create(store, Some(""), "trace:t3", Mode::Strict, 1 << 20, NOW) {
            Err(ReplayError::Unreplayable(message)) => {
                assert!(message.contains("loose"), "{message}")
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        let loose = registry
            .create(store, Some(""), "trace:t3", Mode::Loose, 1 << 20, NOW)
            .expect("loose replays a text prompt");
        let served = registry
            .next_llm(store, &loose.id, Some(""), None, NOW, keep_step)
            .expect("serves");
        let body: Value =
            serde_json::from_slice(&served.anthropic(&json!({})).expect("renders").body).unwrap();
        assert_eq!(body["content"], json!([{"type": "text", "text": "hello"}]));
        assert_eq!(body["stop_reason"], json!("end_turn"));
    }

    #[test]
    fn recorded_answers_read_every_shape() {
        let reasons = ["length".to_owned()];
        // Parsed JSON text is served as the text it was.
        let json_answer = recorded_choices(&json!([1, 2]), ResponseSource::Text, &[]);
        assert_eq!(json_answer[0].text.as_deref(), Some("[1,2]"));
        // And so is a list of non-messages, even from a message source.
        let not_messages = recorded_choices(&json!([1, 2]), ResponseSource::Messages, &[]);
        assert_eq!(not_messages[0].text.as_deref(), Some("[1,2]"));
        // OTel parts.
        let parts = recorded_choices(
            &json!([{"role": "assistant", "parts": [
                {"type": "text", "content": "On it."},
                {"type": "tool_call", "id": "c9", "name": "f", "arguments": {"x": 1}},
            ], "finish_reason": "tool_call"}]),
            ResponseSource::Messages,
            &[],
        );
        assert_eq!(parts[0].text.as_deref(), Some("On it."));
        assert_eq!(parts[0].calls[0].name, "f");
        assert_eq!(openai_finish(&parts[0]), "tool_calls");
        // A raw OpenAI response object.
        let raw = recorded_choices(
            &json!({"choices": [{"message": {"role": "assistant", "content": "a"},
                                 "finish_reason": "stop"},
                                {"message": {"role": "assistant", "content": "b"}}]}),
            ResponseSource::Body,
            &reasons,
        );
        assert_eq!(raw.len(), 2);
        assert_eq!(raw[1].text.as_deref(), Some("b"));
        assert_eq!(
            raw[1].finish_reason.as_deref(),
            None,
            "the second choice has no recorded reason at index 1"
        );
        // A raw Anthropic message.
        let anthropic = recorded_choices(
            &json!({"type": "message", "role": "assistant", "stop_reason": "tool_use",
                    "content": [{"type": "tool_use", "id": "toolu_1", "name": "g",
                                 "input": {"q": "x"}}]}),
            ResponseSource::Body,
            &[],
        );
        assert_eq!(anthropic[0].calls[0].arguments, json!({"q": "x"}));
        assert_eq!(
            anthropic_stop(anthropic[0].finish_reason.as_deref(), true),
            "tool_use"
        );
        // A finish reason recorded only as the span attribute fills in.
        let text = recorded_choices(&json!("done"), ResponseSource::Text, &reasons);
        assert_eq!(openai_finish(&text[0]), "length");
        // Reasoning messages are not part of the answer.
        let thinking = recorded_choices(
            &json!([{"role": "thinking", "content": "hmm"},
                    {"role": "assistant", "content": "yes"}]),
            ResponseSource::Messages,
            &[],
        );
        assert_eq!(thinking.len(), 1);
        assert_eq!(thinking[0].text.as_deref(), Some("yes"));
    }

    #[test]
    fn streams_reassemble_to_the_recorded_text_and_arguments() {
        let step = ServedStep {
            replay: "rp_0123456789abcdef0123456789abcdef".into(),
            step: 4,
            model: Some("gpt-4o".into()),
            response_id: None,
            start_ns: 1_700_000_000_000_000_000,
            choices: vec![Choice {
                text: Some("The quick brown fox jumps over the lazy dog, twice.".into()),
                calls: vec![Call {
                    id: None,
                    name: "lookup".into(),
                    arguments: json!({"term": "fox", "limit": 3}),
                }],
                finish_reason: None,
            }],
            tokens: tape::StepTokens {
                input: Some(5),
                output: Some(7),
                ..tape::StepTokens::default()
            },
            max_response_bytes: MAX_RESPONSE_BYTES,
        };
        let request = json!({"stream": true, "stream_options": {"include_usage": true}});
        let openai = String::from_utf8(step.openai(&request).expect("renders").body).unwrap();
        assert!(openai.ends_with("data: [DONE]\n\n"));
        let mut text = String::new();
        let mut arguments = String::new();
        let mut finish = None;
        let mut usage = None;
        for line in openai
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
        {
            if line == "[DONE]" {
                continue;
            }
            let event: Value = serde_json::from_str(line).unwrap();
            assert_eq!(event["object"], json!("chat.completion.chunk"));
            assert_eq!(event["id"], json!("chatcmpl-traza0123456789abs4"));
            if let Some(choice) = event["choices"].get(0) {
                text.push_str(choice["delta"]["content"].as_str().unwrap_or_default());
                if let Some(piece) =
                    choice["delta"]["tool_calls"][0]["function"]["arguments"].as_str()
                {
                    arguments.push_str(piece);
                }
                if let Some(reason) = choice["finish_reason"].as_str() {
                    finish = Some(reason.to_owned());
                }
            } else {
                usage = Some(event["usage"].clone());
            }
        }
        assert_eq!(text, "The quick brown fox jumps over the lazy dog, twice.");
        assert_eq!(
            serde_json::from_str::<Value>(&arguments).unwrap(),
            json!({"term": "fox", "limit": 3})
        );
        assert_eq!(finish.as_deref(), Some("tool_calls"));
        assert_eq!(usage.unwrap()["total_tokens"], json!(12));

        let anthropic = String::from_utf8(
            step.anthropic(&json!({"stream": true}))
                .expect("renders")
                .body,
        )
        .unwrap();
        let names: Vec<&str> = anthropic
            .lines()
            .filter_map(|line| line.strip_prefix("event: "))
            .collect();
        assert_eq!(names.first(), Some(&"message_start"));
        assert_eq!(names.last(), Some(&"message_stop"));
        let mut text = String::new();
        let mut partial = String::new();
        for line in anthropic
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
        {
            let event: Value = serde_json::from_str(line).unwrap();
            if event["type"] == json!("content_block_delta") {
                text.push_str(event["delta"]["text"].as_str().unwrap_or_default());
                partial.push_str(event["delta"]["partial_json"].as_str().unwrap_or_default());
            }
            if event["type"] == json!("message_delta") {
                assert_eq!(event["delta"]["stop_reason"], json!("tool_use"));
            }
        }
        assert_eq!(text, "The quick brown fox jumps over the lazy dog, twice.");
        assert_eq!(
            serde_json::from_str::<Value>(&partial).unwrap(),
            json!({"term": "fox", "limit": 3})
        );
    }

    #[test]
    fn deltas_cut_after_whitespace_and_bound_their_count() {
        assert_eq!(
            deltas("alpha beta gamma delta"),
            ["alpha beta ", "gamma delta"]
        );
        assert!(deltas("").is_empty());
        let long = "word ".repeat(10_000);
        let pieces = deltas(&long);
        assert!(pieces.len() <= 2 * MAX_STREAM_DELTAS, "{}", pieces.len());
        assert_eq!(pieces.concat(), long);
        // Multi-byte characters are never split.
        let accented = "é".repeat(40);
        assert_eq!(deltas(&accented).concat(), accented);
    }

    /// A one-step run in its own trace and session: a model call and its
    /// answer, under `tenant`.
    fn single(tenant: &str, trace: &str, session: &str) -> Span {
        let mut span = span(
            trace,
            "llm",
            1,
            json!({"session.id": session, "gen_ai.operation.name": "chat",
                   "gen_ai.prompt.0.role": "user", "gen_ai.prompt.0.content": "hi",
                   "gen_ai.completion.0.content": "hello"}),
        );
        span.tenant = tenant.into();
        span
    }

    #[test]
    fn an_erasure_withdraws_every_replay_holding_what_it_erased() {
        let fixture = TempStore::open("replay-erasure", crate::Config::default());
        let store = fixture.store();
        openllmetry_run(store);
        store
            .ingest_batch(vec![single("", "t-other", "run-other")])
            .expect("ingests");
        let registry = Registry::default();
        let create = |run: &str| {
            registry
                .create(store, Some(""), run, Mode::Loose, 1 << 20, NOW)
                .expect("creates")
                .id
        };
        let (held, untouched, tools) = (create("run-1"), create("run-other"), create("run-1"));
        let request = openai_request_digest(&first_request()).expect("parses");
        registry
            .next_llm(store, &held, Some(""), request.clone(), NOW, keep_step)
            .expect("serves before the erasure");

        let status = store
            .erase(Subject::Session {
                session_id: "run-1".into(),
                tenant: String::new(),
            })
            .expect("erases");
        let erasure = status.erase.id;

        // Every route of a replay holding the session refuses from now on,
        // naming the erasure, and keeps refusing.
        for _ in 0..2 {
            match registry.next_llm(store, &held, Some(""), request.clone(), NOW, keep_step) {
                Err(ReplayError::Erased { erasure: named }) => assert_eq!(named, erasure),
                other => panic!("expected the replay withdrawn, got {other:?}"),
            }
        }
        assert!(matches!(
            registry.status(store, &held, Some(""), NOW),
            Err(ReplayError::Erased { .. })
        ));
        assert!(matches!(
            registry.next_tool(store, &tools, Some(""), None, NOW),
            Err(ReplayError::Erased { .. })
        ));
        // A foreign caller still learns nothing: not found, not erased.
        assert!(matches!(
            registry.status(store, &held, Some("acme"), NOW),
            Err(ReplayError::NotFound)
        ));
        // A replay of other data keeps serving.
        registry
            .next_llm(store, &untouched, Some(""), None, NOW, keep_step)
            .expect("an unrelated replay is not withdrawn");
        // A replay created AFTER the erasure is new data, served normally —
        // and the run it names is gone, so there is nothing to create.
        assert!(matches!(
            registry.create(store, Some(""), "run-1", Mode::Loose, 1 << 20, NOW),
            Err(ReplayError::RunNotFound)
        ));
        // Withdrawn replays hold nothing and count against no bound.
        assert_eq!(registry.len(NOW), 3);
    }

    #[test]
    fn payload_tenant_and_span_erasures_reach_replays_too() {
        let config = crate::Config {
            payload_threshold: Some(64),
            ..crate::Config::default()
        };
        let fixture = TempStore::open("replay-erasure-kinds", config);
        let store = fixture.store();
        let long_answer = "a".repeat(200);
        let mut offloaded = single("", "t-pay", "s-pay");
        offloaded
            .attributes
            .insert("gen_ai.completion.0.content".into(), json!(long_answer));
        store
            .ingest_batch(vec![
                offloaded,
                single("acme", "t-acme", "s-acme"),
                single("", "t-span", "s-span"),
            ])
            .expect("ingests");
        let registry = Registry::default();
        let paid = registry
            .create(store, Some(""), "s-pay", Mode::Loose, 1 << 20, NOW)
            .expect("creates")
            .id;
        let acme = registry
            .create(store, Some("acme"), "s-acme", Mode::Loose, 1 << 20, NOW)
            .expect("creates")
            .id;
        let spanned = registry
            .create(store, Some(""), "s-span", Mode::Loose, 1 << 20, NOW)
            .expect("creates")
            .id;
        let tape = store
            .tape(Some(""), "s-pay", TapeOptions::default())
            .expect("reads")
            .expect("exists");
        let mut references = HashSet::new();
        tape::collect_references(tape.steps[0].response.as_ref().unwrap(), &mut references);
        let reference = references.into_iter().next().expect("offloaded");

        store
            .erase(Subject::Payload { reference })
            .expect("erases the payload");
        store
            .erase(Subject::Tenant {
                tenant: "acme".into(),
            })
            .expect("erases the tenant");
        store
            .erase(Subject::Span {
                trace_id: "t-span".into(),
                span_id: "llm".into(),
                tenant: String::new(),
            })
            .expect("erases the span");
        for (id, binding) in [(&paid, Some("")), (&acme, Some("acme")), (&spanned, None)] {
            assert!(
                matches!(
                    registry.next_llm(store, id, binding, None, NOW, keep_step),
                    Err(ReplayError::Erased { .. })
                ),
                "{id}"
            );
        }
    }

    #[test]
    fn each_tenant_has_its_own_share_of_the_registry() {
        let fixture = TempStore::open("replay-shares", crate::Config::default());
        let store = fixture.store();
        store
            .ingest_batch(vec![
                single("acme", "t-a", "s-a"),
                single("beta", "t-b", "s-b"),
            ])
            .expect("ingests");
        let registry = Registry::new(Limits {
            ttl: Duration::from_secs(10),
            max_live_per_tenant: 2,
            ..Limits::default()
        });
        let create = |tenant: Option<&str>, run: &str, at: u64| {
            registry.create(store, tenant, run, Mode::Loose, 1 << 20, at)
        };
        create(Some("acme"), "s-a", NOW).expect("first");
        create(Some("acme"), "s-a", NOW + 2_000_000_000).expect("second");
        // Acme's share is full; the retry time is its own oldest replay's.
        match create(Some("acme"), "s-a", NOW + 3_000_000_000) {
            Err(ReplayError::Full {
                own_share: true,
                max_live: 2,
                retry_after_seconds,
                ..
            }) => assert_eq!(retry_after_seconds, 7),
            other => panic!("expected acme's share full, got {other:?}"),
        }
        // Beta, and the operator view, are untouched by it.
        create(Some("beta"), "s-b", NOW).expect("beta has its own share");
        create(None, "s-b", NOW).expect("so does the operator view");
        // Once acme's oldest expires, it has room again.
        create(Some("acme"), "s-a", NOW + 10_000_000_000).expect("room after expiry");
    }

    #[test]
    fn a_snapshot_larger_than_a_share_is_refused_for_good() {
        let fixture = TempStore::open("replay-too-large", crate::Config::default());
        let store = fixture.store();
        openllmetry_run(store);
        let registry = Registry::new(Limits {
            max_bytes_per_tenant: 300,
            ..Limits::default()
        });
        match registry.create(store, Some(""), "run-1", Mode::Loose, 1 << 20, NOW) {
            Err(error @ ReplayError::TooLarge { bytes, limit: 300 }) => {
                assert!(bytes > 300);
                assert!(
                    error.to_string().contains("can never be created"),
                    "{error}"
                );
            }
            other => panic!("expected a permanent refusal, got {other:?}"),
        }
    }

    #[test]
    fn retry_after_waits_until_enough_room_is_freed() {
        let second = 1_000_000_000;
        // Room now: no wait.
        assert_eq!(
            wait_for_room(vec![(NOW + second, 10)], 2, 100, 50, NOW),
            None
        );
        // The count bound: the oldest expiry frees a slot.
        assert_eq!(
            wait_for_room(
                vec![(NOW + 5 * second, 1), (NOW + 3 * second, 1)],
                2,
                100,
                1,
                NOW
            ),
            Some(3)
        );
        // The byte bound: the oldest expiry frees too little, so the answer
        // is the expiry after which the snapshot fits — not the oldest.
        assert_eq!(
            wait_for_room(
                vec![(NOW + 2 * second, 10), (NOW + 9 * second, 80)],
                10,
                100,
                50,
                NOW
            ),
            Some(9)
        );
        // Never less than a second.
        assert_eq!(wait_for_room(vec![(NOW + 1, 1)], 1, 100, 1, NOW), Some(1));
    }

    #[test]
    fn a_run_longer_than_the_span_cap_is_refused_not_cut() {
        let fixture = TempStore::open("replay-span-cap", crate::Config::default());
        let store = fixture.store();
        openllmetry_run(store);
        let registry = Registry::new(Limits {
            max_spans: 2,
            ..Limits::default()
        });
        match registry.create(store, Some(""), "run-1", Mode::Loose, 1 << 20, NOW) {
            Err(ReplayError::Unreplayable(message)) => {
                assert!(message.contains("more than 2 spans"), "{message}")
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        let fits = Registry::new(Limits {
            max_spans: 3,
            ..Limits::default()
        });
        fits.create(store, Some(""), "run-1", Mode::Loose, 1 << 20, NOW)
            .expect("a run exactly at the cap replays");
    }

    #[test]
    fn only_what_a_replay_holds_is_charged_against_the_tape_budget() {
        // A 64-byte threshold offloads every long value below.
        let config = crate::Config {
            payload_threshold: Some(64),
            ..crate::Config::default()
        };
        let fixture = TempStore::open("replay-budget", config);
        let store = fixture.store();
        let history = |turns: usize| {
            let messages: Vec<Value> = (0..turns)
                .map(|turn| {
                    json!({"role": "user", "parts": [
                    {"type": "text", "content": format!("{turn}: {}", "x".repeat(120))}]})
                })
                .collect();
            serde_json::to_string(&messages).expect("encodes")
        };
        let mut spans = Vec::new();
        // A conversation whose whole message list is one offloaded attribute
        // that grows at every step — the OTel GenAI shape — with short,
        // inline answers.
        for turn in 1..=6 {
            spans.push(span(
                "t-long",
                &format!("llm-{turn}"),
                turn as u64 * 10,
                json!({"session.id": "long", "gen_ai.operation.name": "chat",
                       "gen_ai.input.messages": history(turn),
                       "gen_ai.completion.0.role": "assistant",
                       "gen_ai.completion.0.content": format!("ok {turn}")}),
            ));
        }
        // A tool whose result is offloaded and larger than the budget.
        spans.push(span(
            "t-long",
            "tool",
            100,
            json!({"session.id": "long", "gen_ai.operation.name": "execute_tool",
                   "gen_ai.tool.name": "fetch", "gen_ai.tool.call.arguments": "{\"u\":1}",
                   "gen_ai.tool.call.result": "r".repeat(400)}),
        ));
        store.ingest_batch(spans).expect("ingests");
        let registry = Registry::default();

        // 300 bytes would not hold even the second request (the six come to
        // over 3 KB); requests are digested, not held, so strict creation
        // succeeds.
        let created = registry
            .create(store, Some(""), "long", Mode::Strict, 300, NOW)
            .expect("requests are not charged");
        assert_eq!(created.llm_steps, 6);
        // The over-budget tool result is flagged, not served as a reference.
        assert_eq!(created.unreadable_tool_steps, [6]);
        match registry.next_tool(store, &created.id, Some(""), Some("fetch"), NOW) {
            Err(ReplayError::ToolUnreadable {
                step: 6, reason, ..
            }) => {
                assert!(reason.contains("past --tape-max-bytes"), "{reason}")
            }
            other => panic!("expected the tool step refused, got {other:?}"),
        }
        assert!(
            matches!(
                registry.next_tool(store, &created.id, Some(""), None, NOW),
                Err(ReplayError::ToolUnreadable { step: 6, .. })
            ),
            "a refused tool step is not consumed"
        );

        // An offloaded ANSWER past the budget refuses creation, and says the
        // budget is why.
        store
            .ingest_batch(vec![span(
                "t-big",
                "llm",
                1,
                json!({"gen_ai.operation.name": "chat", "gen_ai.prompt.0.content": "hi",
                       "gen_ai.completion.0.content": "z".repeat(400)}),
            )])
            .expect("ingests");
        match registry.create(store, Some(""), "trace:t-big", Mode::Loose, 300, NOW) {
            Err(ReplayError::Unreplayable(message)) => {
                assert!(message.contains("past --tape-max-bytes"), "{message}")
            }
            other => panic!("expected a budget refusal, got {other:?}"),
        }
        registry
            .create(store, Some(""), "trace:t-big", Mode::Loose, 1 << 20, NOW)
            .expect("a larger budget holds it");
    }

    // ------------------------------------------------- review regressions

    /// A recorded answer with no recorded model, as the stream renders it.
    fn modelless(text: String) -> ServedStep {
        ServedStep {
            replay: "rp_0123456789abcdef0123456789abcdef".into(),
            step: 0,
            model: None,
            response_id: None,
            start_ns: 1_700_000_000_000_000_000,
            choices: vec![text_choice(text)],
            tokens: tape::StepTokens::default(),
            max_response_bytes: MAX_RESPONSE_BYTES,
        }
    }

    #[test]
    fn a_request_model_cannot_amplify_a_stream() {
        // Every chunk repeats the model. Echoing a request's model of any
        // length would make one request's bytes come back once per chunk.
        let step = modelless("word ".repeat(820));
        let stream = |request: Value| step.openai(&request).expect("renders").body;
        let huge = stream(json!({"stream": true, "model": "m".repeat(64 * 1024)}));
        let none = stream(json!({"stream": true}));
        assert_eq!(huge, none, "a model no provider would name is not echoed");
        assert!(none.len() < 128 * 1024, "{}", none.len());
        // A model of a real length still is.
        let named = String::from_utf8(stream(json!({"stream": true, "model": "gpt-4o"}))).unwrap();
        assert!(named.contains("\"model\":\"gpt-4o\""));

        // Nor is a recorded model or response id that long: the replay keeps
        // neither, and serves an invented id and the request's model.
        let fixture = TempStore::open("replay-identifiers", crate::Config::default());
        let store = fixture.store();
        store
            .ingest_batch(vec![span(
                "t-ids",
                "llm",
                1,
                json!({"gen_ai.operation.name": "chat", "gen_ai.prompt.0.content": "hi",
                       "gen_ai.request.model": "r".repeat(4096),
                       "gen_ai.response.id": "i".repeat(4096),
                       "gen_ai.completion.0.content": "ok"}),
            )])
            .expect("ingests");
        let registry = Registry::default();
        let id = registry
            .create(store, Some(""), "trace:t-ids", Mode::Loose, 1 << 20, NOW)
            .expect("creates")
            .id;
        let rendered = registry
            .next_llm(store, &id, Some(""), None, NOW, |step: &ServedStep| {
                step.openai(&json!({"model": "gpt-4o"}))
            })
            .expect("serves");
        let body: Value = serde_json::from_slice(&rendered.body).unwrap();
        assert_eq!(body["model"], json!("gpt-4o"));
        assert!(
            body["id"].as_str().unwrap().starts_with("chatcmpl-traza"),
            "{}",
            body["id"]
        );
    }

    #[test]
    fn a_response_past_its_bound_is_refused_without_consuming_the_step() {
        let fixture = TempStore::open("replay-response-cap", crate::Config::default());
        let store = fixture.store();
        let answer = "token ".repeat(500);
        store
            .ingest_batch(vec![span(
                "t-cap",
                "llm",
                1,
                json!({"gen_ai.operation.name": "chat", "gen_ai.prompt.0.content": "hi",
                       "gen_ai.completion.0.content": answer}),
            )])
            .expect("ingests");
        // Room for the answer as one body, not for it chunked into a stream.
        let registry = Registry::new(Limits {
            max_response_bytes: 8 * 1024,
            ..Limits::default()
        });
        let id = registry
            .create(store, Some(""), "trace:t-cap", Mode::Loose, 1 << 20, NOW)
            .expect("creates")
            .id;
        let render = |request: Value| move |step: &ServedStep| step.openai(&request);
        for request in [
            json!({"stream": true}),
            json!({"stream": true, "stream_options": {"include_usage": true}}),
        ] {
            match registry.next_llm(store, &id, Some(""), None, NOW, render(request)) {
                Err(error @ ReplayError::ResponseTooLarge { step: 0, limit }) => {
                    assert_eq!(limit, 8 * 1024);
                    assert!(error.to_string().contains("not consumed"), "{error}");
                }
                other => panic!("expected the stream refused, got {other:?}"),
            }
        }
        let status = registry.status(store, &id, Some(""), NOW).expect("live");
        assert_eq!(status["served"], json!(0), "nothing was consumed");
        let rendered = registry
            .next_llm(store, &id, Some(""), None, NOW, render(json!({})))
            .expect("the same step, unstreamed, fits");
        assert!(rendered.body.len() <= 8 * 1024);
        let body: Value = serde_json::from_slice(&rendered.body).unwrap();
        assert_eq!(body["choices"][0]["message"]["content"], json!(answer));
        // The Anthropic stream is bounded the same way.
        let anthropic = modelless(answer.clone());
        let capped = ServedStep {
            max_response_bytes: 4 * 1024,
            ..anthropic
        };
        assert!(matches!(
            capped.anthropic(&json!({"stream": true})),
            Err(ReplayError::ResponseTooLarge { .. })
        ));
    }

    #[test]
    fn everything_a_replay_retains_is_charged_to_its_share() {
        let fixture = TempStore::open("replay-charges", crate::Config::default());
        let store = fixture.store();
        let mut spans = vec![span(
            "t-names",
            "llm",
            1,
            json!({"gen_ai.operation.name": "chat", "gen_ai.prompt.0.content": "hi",
                   "gen_ai.completion.0.content": "ok"}),
        )];
        // Tiny records under long names: the names are what the replay holds.
        for index in 0..40 {
            spans.push(span(
                "t-names",
                &format!("tool-{index:02}"),
                10 + index,
                json!({"gen_ai.operation.name": "execute_tool",
                       "gen_ai.tool.name": format!("{index:02}{}", "n".repeat(8 * 1024)),
                       "gen_ai.tool.call.arguments": "{}", "gen_ai.tool.call.result": "ok"}),
            ));
        }
        store.ingest_batch(spans).expect("ingests");
        // Room for a replay's fixed costs (below) and 64 KiB more.
        let calls = MAX_RECORDED_CALLS * std::mem::size_of::<CallRecord>();
        let share = calls + 64 * 1024;
        let registry = Registry::new(Limits {
            max_bytes_per_tenant: share,
            ..Limits::default()
        });
        match registry.create(store, Some(""), "trace:t-names", Mode::Loose, 1 << 20, NOW) {
            Err(ReplayError::TooLarge { bytes, limit }) => {
                assert_eq!(limit, share);
                assert!(bytes > limit, "{bytes}");
                // Building stopped once past the share, not at the end.
                assert!(bytes < limit + 32 * 1024, "{bytes}");
            }
            other => panic!("320 KiB of tool names fit in 64 KiB: {other:?}"),
        }
        // The call log a replay may grow to is charged up front, so growth
        // after admission cannot take a share past its budget.
        store
            .ingest_batch(vec![single("", "t-tiny", "s-tiny")])
            .expect("ingests");
        let tight = Registry::new(Limits {
            max_bytes_per_tenant: calls,
            ..Limits::default()
        });
        assert!(matches!(
            tight.create(store, Some(""), "s-tiny", Mode::Loose, 1 << 20, NOW),
            Err(ReplayError::TooLarge { .. })
        ));
        let roomy = Registry::new(Limits {
            max_bytes_per_tenant: calls + 16 * 1024,
            ..Limits::default()
        });
        roomy
            .create(store, Some(""), "s-tiny", Mode::Loose, 1 << 20, NOW)
            .expect("a one-step replay fits beside its call log");
    }

    #[test]
    fn withdrawn_replays_are_bounded_too() {
        let fixture = TempStore::open("replay-tombstones", crate::Config::default());
        let store = fixture.store();
        let registry = Registry::new(Limits {
            max_live: 4,
            max_live_per_tenant: 2,
            ..Limits::default()
        });
        let mut withdrawn = Vec::new();
        for round in 0..10u64 {
            let at = NOW + round;
            let session = format!("s-gone-{round}");
            store
                .ingest_batch(vec![single("", &format!("t-gone-{round}"), &session)])
                .expect("ingests");
            let id = registry
                .create(store, Some(""), &session, Mode::Loose, 1 << 20, at)
                .expect("a withdrawn replay frees its slot")
                .id;
            store
                .erase(Subject::Session {
                    session_id: session,
                    tenant: String::new(),
                })
                .expect("erases");
            assert!(matches!(
                registry.status(store, &id, Some(""), at),
                Err(ReplayError::Erased { .. })
            ));
            withdrawn.push(id);
        }
        let later = NOW + 10;
        assert!(
            registry.len(later) <= 4,
            "{} entries for a share of two",
            registry.len(later)
        );
        // The newest withdrawals still say why; the one closest to expiring
        // is forgotten first.
        for id in &withdrawn[8..] {
            assert!(matches!(
                registry.status(store, id, Some(""), later),
                Err(ReplayError::Erased { .. })
            ));
        }
        assert!(matches!(
            registry.status(store, &withdrawn[0], Some(""), later),
            Err(ReplayError::NotFound)
        ));
        // Withdrawn replays take no live replay's room.
        store
            .ingest_batch(vec![single("", "t-live", "s-live")])
            .expect("ingests");
        for _ in 0..2 {
            registry
                .create(store, Some(""), "s-live", Mode::Loose, 1 << 20, later)
                .expect("the share's live slots are free");
        }
    }

    #[test]
    fn offloaded_messages_keep_their_separate_system_instructions() {
        let config = crate::Config {
            payload_threshold: Some(64),
            ..crate::Config::default()
        };
        let fixture = TempStore::open("replay-system", config);
        let store = fixture.store();
        let question = format!("Summarise: {}", "x".repeat(120));
        let messages = json!([{"role": "user", "parts": [{"type": "text", "content": question}]}]);
        store
            .ingest_batch(vec![span(
                "t-sys",
                "llm",
                1,
                json!({"gen_ai.operation.name": "chat",
                       "gen_ai.system_instructions": "Be brief.",
                       "gen_ai.input.messages": serde_json::to_string(&messages).unwrap(),
                       "gen_ai.completion.0.content": "Done."}),
            )])
            .expect("ingests");
        let registry = Registry::default();
        let id = registry
            .create(store, Some(""), "trace:t-sys", Mode::Strict, 1 << 20, NOW)
            .expect("creates")
            .id;
        let without = openai_request_digest(&json!({"messages": [
            {"role": "user", "content": question}]}))
        .expect("parses");
        assert!(
            matches!(
                registry.next_llm(store, &id, Some(""), without, NOW, keep_step),
                Err(ReplayError::Diverged { .. })
            ),
            "a request without the recorded system prompt is a different request"
        );
        let with = openai_request_digest(&json!({"messages": [
            {"role": "system", "content": "Be brief."},
            {"role": "user", "content": question}]}))
        .expect("parses");
        registry
            .next_llm(store, &id, Some(""), with, NOW, keep_step)
            .expect("the request as recorded matches");
    }

    #[test]
    fn a_tool_result_never_captured_is_refused_not_served_as_null() {
        let fixture = TempStore::open("replay-uncaptured", crate::Config::default());
        let store = fixture.store();
        store
            .ingest_batch(vec![
                span(
                    "t-cap",
                    "llm",
                    1,
                    json!({"gen_ai.operation.name": "chat", "gen_ai.prompt.0.content": "hi",
                           "gen_ai.completion.0.content": "ok"}),
                ),
                // Content capture off: the call is recorded, its result not.
                span(
                    "t-cap",
                    "blind",
                    2,
                    json!({"gen_ai.operation.name": "execute_tool",
                           "gen_ai.tool.name": "fetch", "gen_ai.tool.call.arguments": "{\"u\":1}"}),
                ),
                // A result recorded as empty is a result.
                span(
                    "t-cap",
                    "empty",
                    3,
                    json!({"gen_ai.operation.name": "execute_tool",
                           "gen_ai.tool.name": "clear", "gen_ai.tool.call.result": ""}),
                ),
                span(
                    "t-cap",
                    "null",
                    4,
                    json!({"gen_ai.operation.name": "execute_tool",
                           "gen_ai.tool.name": "reset", "gen_ai.tool.call.result": null}),
                ),
            ])
            .expect("ingests");
        let registry = Registry::default();
        let created = registry
            .create(store, Some(""), "trace:t-cap", Mode::Loose, 1 << 20, NOW)
            .expect("creates");
        assert_eq!(created.unreadable_tool_steps, [1]);
        for _ in 0..2 {
            match registry.next_tool(store, &created.id, Some(""), Some("fetch"), NOW) {
                Err(ReplayError::ToolUnreadable {
                    step: 1, reason, ..
                }) => {
                    assert!(reason.contains("recorded no result"), "{reason}")
                }
                other => panic!("expected the uncaptured result refused, got {other:?}"),
            }
        }
        let empty = registry
            .next_tool(store, &created.id, Some(""), Some("clear"), NOW)
            .expect("an empty result is served");
        assert_eq!(empty.result, json!(""));
        let null = registry
            .next_tool(store, &created.id, Some(""), Some("reset"), NOW)
            .expect("a null result is served");
        assert_eq!(null.result, Value::Null);
    }

    #[test]
    fn answer_text_that_reads_as_json_is_served_as_that_text() {
        let fixture = TempStore::open("replay-provenance", crate::Config::default());
        let store = fixture.store();
        let answers = [
            "{\"content\":\"hello\",\"count\":2}",
            "{\"choices\":[{\"message\":{\"content\":\"x\"}}]}",
            "{\"role\":\"user\"}",
        ];
        let mut spans: Vec<Span> = answers
            .iter()
            .enumerate()
            .map(|(index, answer)| {
                span(
                    &format!("t-text-{index}"),
                    "llm",
                    1,
                    json!({"gen_ai.operation.name": "chat", "llm.prompt": "hi",
                           "llm.completion": answer}),
                )
            })
            .collect();
        // OpenInference says which `output.value` is text.
        spans.push(span(
            "t-text-oi",
            "llm",
            1,
            json!({"openinference.span.kind": "LLM", "input.value": "hi",
                   "output.value": answers[0], "output.mime_type": "text/plain"}),
        ));
        store.ingest_batch(spans).expect("ingests");
        let registry = Registry::default();
        let traces = ["t-text-0", "t-text-1", "t-text-2", "t-text-oi"];
        for (trace, answer) in traces.iter().zip(answers.iter().chain([&answers[0]])) {
            let id = registry
                .create(
                    store,
                    Some(""),
                    &format!("trace:{trace}"),
                    Mode::Loose,
                    1 << 20,
                    NOW,
                )
                .expect("creates")
                .id;
            let served = registry
                .next_llm(store, &id, Some(""), None, NOW, keep_step)
                .expect("serves");
            let body: Value =
                serde_json::from_slice(&served.openai(&json!({})).expect("renders").body).unwrap();
            let text = body["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or("");
            assert_eq!(
                serde_json::from_str::<Value>(text).ok(),
                serde_json::from_str::<Value>(answer).ok(),
                "{trace}: served {text:?}"
            );
        }
        // An OpenInference JSON body is still the provider's envelope.
        store
            .ingest_batch(vec![span(
                "t-body",
                "llm",
                1,
                json!({"openinference.span.kind": "LLM", "input.value": "hi",
                       "output.mime_type": "application/json",
                       "output.value": "{\"choices\":[{\"message\":{\"role\":\"assistant\",\"content\":\"from the body\"}}]}"}),
            )])
            .expect("ingests");
        let id = registry
            .create(store, Some(""), "trace:t-body", Mode::Loose, 1 << 20, NOW)
            .expect("creates")
            .id;
        let served = registry
            .next_llm(store, &id, Some(""), None, NOW, keep_step)
            .expect("serves");
        let body: Value =
            serde_json::from_slice(&served.openai(&json!({})).expect("renders").body).unwrap();
        assert_eq!(
            body["choices"][0]["message"]["content"],
            json!("from the body")
        );
    }

    #[test]
    fn answer_text_is_served_as_written_not_reserialized() {
        // A 64-byte threshold offloads the long answers below.
        let config = crate::Config {
            payload_threshold: Some(64),
            ..crate::Config::default()
        };
        let fixture = TempStore::open("replay-raw-text", config);
        let store = fixture.store();
        // Duplicate keys, spacing, key order and number spelling are all
        // part of the text, and all lost by a parse and re-serialization.
        let duplicate = "{\"content\":\"first\",\"content\":\"second\"}".to_owned();
        let spelled = "{ \"b\": 1.50,\n  \"a\": [ 1e2 ] }".to_owned();
        let long = format!("{{\"z\": \"{}\",  \"a\": 1.0}}", "x".repeat(100));
        let body = "{\"score\": 1.50,  \"label\": \"ok\"}".to_owned();
        let text = |answer: &str| {
            json!({"gen_ai.operation.name": "chat", "llm.prompt": "hi",
                                         "llm.completion": answer})
        };
        let openinference = |answer: &str, mime: Option<&str>| {
            let mut attributes = json!({"openinference.span.kind": "LLM", "input.value": "hi",
                                        "output.value": answer});
            if let Some(mime) = mime {
                attributes["output.mime_type"] = json!(mime);
            }
            attributes
        };
        let cases = [
            ("t-dup", text(&duplicate), &duplicate),
            ("t-spelled", text(&spelled), &spelled),
            ("t-offloaded", text(&long), &long),
            (
                "t-oi-text",
                openinference(&spelled, Some("text/plain")),
                &spelled,
            ),
            // A provider body that holds no envelope is served as text too.
            ("t-oi-body", openinference(&body, None), &body),
            ("t-oi-body-offloaded", openinference(&long, None), &long),
        ];
        store
            .ingest_batch(
                cases
                    .iter()
                    .map(|(trace, attributes, _)| span(trace, "llm", 1, attributes.clone()))
                    .collect(),
            )
            .expect("ingests");
        let registry = Registry::default();
        for (trace, _, answer) in &cases {
            let id = registry
                .create(
                    store,
                    Some(""),
                    &format!("trace:{trace}"),
                    Mode::Loose,
                    1 << 20,
                    NOW,
                )
                .expect("creates")
                .id;
            let served = registry
                .next_llm(store, &id, Some(""), None, NOW, keep_step)
                .expect("serves");
            let openai: Value =
                serde_json::from_slice(&served.openai(&json!({})).expect("renders").body).unwrap();
            assert_eq!(
                openai["choices"][0]["message"]["content"],
                json!(answer),
                "{trace}"
            );
            let anthropic: Value =
                serde_json::from_slice(&served.anthropic(&json!({})).expect("renders").body)
                    .unwrap();
            assert_eq!(anthropic["content"][0]["text"], json!(answer), "{trace}");
        }
    }

    #[test]
    fn strict_replay_refuses_a_bare_prompt_beside_system_instructions() {
        let config = crate::Config {
            payload_threshold: Some(64),
            ..crate::Config::default()
        };
        let fixture = TempStore::open("replay-bare-prompt", config);
        let store = fixture.store();
        let question = format!("What is 2+2? {}", "q".repeat(100));
        let prompts = [
            // A JSON object prompt, inline and offloaded.
            ("t-object", json!({"question": "What is 2+2?"}).to_string()),
            (
                "t-object-offloaded",
                json!({"question": question}).to_string(),
            ),
            // One that happens to read as a message is still not a list of
            // them: what the agent sent is not known to be messages.
            (
                "t-message",
                json!({"role": "user", "content": "What is 2+2?"}).to_string(),
            ),
            (
                "t-message-offloaded",
                json!({"role": "user", "content": question}).to_string(),
            ),
            // A list whose element is no message digests as if it were absent.
            ("t-list", json!([{"question": "What is 2+2?"}]).to_string()),
        ];
        store
            .ingest_batch(
                prompts
                    .iter()
                    .map(|(trace, prompt)| {
                        span(
                            trace,
                            "llm",
                            1,
                            json!({"gen_ai.operation.name": "chat",
                                   "gen_ai.system_instructions": "Be brief.",
                                   "input.value": prompt, "llm.completion": "4"}),
                        )
                    })
                    .collect(),
            )
            .expect("ingests");
        // The request the bug let through: the system message alone.
        let system_only = openai_request_digest(&json!({"messages": [
            {"role": "system", "content": "Be brief."}]}))
        .expect("parses");
        let registry = Registry::default();
        for (trace, _) in &prompts {
            let run = format!("trace:{trace}");
            match registry.create(store, Some(""), &run, Mode::Strict, 1 << 20, NOW) {
                Err(ReplayError::Unreplayable(message)) => {
                    assert!(message.contains("loose"), "{trace}: {message}")
                }
                other => panic!("{trace}: expected a strict refusal, got {other:?}"),
            }
            let id = registry
                .create(store, Some(""), &run, Mode::Loose, 1 << 20, NOW)
                .expect("loose replays it")
                .id;
            registry
                .next_llm(store, &id, Some(""), system_only.clone(), NOW, keep_step)
                .expect("loose serves");
            let status = registry.status(store, &id, Some(""), NOW).expect("live");
            assert_eq!(status["calls"][0]["matched"], json!(false), "{trace}");
        }
    }

    #[test]
    fn concurrent_calls_render_each_consumed_step_once() {
        const CALLS: usize = 8;
        let fixture = TempStore::open("replay-concurrent", crate::Config::default());
        let store = fixture.store();
        store
            .ingest_batch(
                (0..CALLS)
                    .map(|index| {
                        span(
                            "t-par",
                            &format!("llm-{index}"),
                            1 + index as u64,
                            json!({"gen_ai.operation.name": "chat",
                                   "gen_ai.prompt.0.content": format!("q{index}"),
                                   "gen_ai.completion.0.content": format!("a{index}")}),
                        )
                    })
                    .collect(),
            )
            .expect("ingests");
        let registry = Registry::default();
        let id = registry
            .create(store, Some(""), "trace:t-par", Mode::Loose, 1 << 20, NOW)
            .expect("creates")
            .id;
        // A failed rendering consumes nothing, and renders nothing twice.
        assert!(matches!(
            registry.next_llm(store, &id, Some(""), None, NOW, |_: &ServedStep| {
                Err::<(), _>(ReplayError::Invalid("refused".to_owned()))
            }),
            Err(ReplayError::Invalid(_))
        ));
        let renders = AtomicU64::new(0);
        let start = std::sync::Barrier::new(CALLS);
        let served: Vec<usize> = std::thread::scope(|scope| {
            let calls: Vec<_> = (0..CALLS)
                .map(|_| {
                    scope.spawn(|| {
                        start.wait();
                        registry
                            .next_llm(store, &id, Some(""), None, NOW, |step: &ServedStep| {
                                renders.fetch_add(1, Ordering::SeqCst);
                                // Long enough that every call is in flight
                                // before the first one commits — the overlap
                                // in which an optimistic loser renders again.
                                std::thread::sleep(Duration::from_millis(20));
                                Ok(step.step)
                            })
                            .expect("serves")
                    })
                })
                .collect();
            calls
                .into_iter()
                .map(|call| call.join().expect("joins"))
                .collect()
        });
        let mut steps = served.clone();
        steps.sort_unstable();
        assert_eq!(
            steps,
            (0..CALLS).collect::<Vec<_>>(),
            "each step served once"
        );
        assert_eq!(
            renders.load(Ordering::SeqCst),
            CALLS as u64,
            "each consumed step rendered once"
        );
        let status = registry.status(store, &id, Some(""), NOW).expect("live");
        assert_eq!(status["served"], json!(CALLS));

        // An erasure recorded while a step renders is caught at the commit:
        // the rendering is dropped unserved and the replay withdrawn.
        let erased = registry
            .create(store, Some(""), "trace:t-par", Mode::Loose, 1 << 20, NOW)
            .expect("creates")
            .id;
        let outcome = registry.next_llm(store, &erased, Some(""), None, NOW, |_: &ServedStep| {
            store
                .erase(Subject::Trace {
                    trace_id: "t-par".to_owned(),
                    tenant: String::new(),
                })
                .expect("erases");
            Ok(())
        });
        assert!(
            matches!(outcome, Err(ReplayError::Erased { .. })),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_call_that_waits_past_the_expiry_is_refused_without_rendering() {
        // Call A renders while call B waits for the replay's turn. Both
        // arrived a second before the replay expires, and A's rendering runs
        // on past the expiry. Expiry is judged when a call reaches the
        // replay, not when its request arrived, so B is refused when its
        // turn comes, rendering nothing and consuming nothing. Two cases,
        // because each check has to hold on its own:
        // - A's rendering fails: A never commits, so nothing but B's own
        //   check once its turn comes stands between B and the replay.
        // - A's rendering succeeds: its commit, judged after the expiry,
        //   drops it unserved — which also drops the replay before B's turn.
        let fixture = TempStore::open("replay-queued-expiry", crate::Config::default());
        let store = fixture.store();
        store
            .ingest_batch(
                (0..2)
                    .map(|index| {
                        span(
                            "t-exp",
                            &format!("llm-{index}"),
                            1 + index,
                            json!({"gen_ai.operation.name": "chat",
                                   "gen_ai.prompt.0.content": format!("q{index}"),
                                   "gen_ai.completion.0.content": format!("a{index}")}),
                        )
                    })
                    .collect(),
            )
            .expect("ingests");
        let ttl = Duration::from_secs(10);
        let arrival = NOW + ttl.as_nanos() as u64 - 1_000_000_000;
        let wait_for = |what: &str, ready: &dyn Fn() -> bool| {
            let started = Instant::now();
            while !ready() {
                assert!(started.elapsed() < Duration::from_secs(10), "{what}");
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        for first_renders in [false, true] {
            let elapsed = Arc::new(AtomicU64::new(0));
            let clock: Clock = {
                let elapsed = Arc::clone(&elapsed);
                Arc::new(move || Duration::from_nanos(elapsed.load(Ordering::SeqCst)))
            };
            let registry = Registry::with_clock(
                Limits {
                    ttl,
                    ..Limits::default()
                },
                clock,
            );
            let id = registry
                .create(store, Some(""), "trace:t-exp", Mode::Loose, 1 << 20, NOW)
                .expect("creates")
                .id;
            // Handles on the replay's turn: the registry's own, plus one per
            // call that has passed its arrival check and is taking or
            // awaiting it.
            let turn_handles = || {
                registry
                    .lock()
                    .get(&id)
                    .map_or(0, |replay| Arc::strong_count(&replay.serving))
            };
            let rendering = std::sync::atomic::AtomicBool::new(false);
            let queued_renders = AtomicU64::new(0);
            let (first, queued) = std::thread::scope(|scope| {
                let first = scope.spawn(|| {
                    registry.next_llm(store, &id, Some(""), None, arrival, |step: &ServedStep| {
                        rendering.store(true, Ordering::SeqCst);
                        wait_for("the second call never queued", &|| turn_handles() == 3);
                        // The rendering outlasts the replay, by a second.
                        elapsed.fetch_add(2_000_000_000, Ordering::SeqCst);
                        match first_renders {
                            true => Ok(step.step),
                            false => Err(ReplayError::Invalid("refused".to_owned())),
                        }
                    })
                });
                wait_for("the first call never rendered", &|| {
                    rendering.load(Ordering::SeqCst)
                });
                let queued = scope.spawn(|| {
                    registry.next_llm(store, &id, Some(""), None, arrival, |step: &ServedStep| {
                        queued_renders.fetch_add(1, Ordering::SeqCst);
                        Ok(step.step)
                    })
                });
                (first.join().expect("joins"), queued.join().expect("joins"))
            });
            let case = match first_renders {
                true => "after a rendering that finished",
                false => "after a rendering that failed",
            };
            assert!(
                matches!(queued, Err(ReplayError::NotFound)),
                "{case}: the queued call was served after the expiry: {queued:?}"
            );
            assert_eq!(
                queued_renders.load(Ordering::SeqCst),
                0,
                "{case}: the queued call rendered a step of an expired replay"
            );
            match first_renders {
                true => assert!(
                    matches!(first, Err(ReplayError::NotFound)),
                    "a rendering finished after the expiry was served: {first:?}"
                ),
                false => assert!(matches!(first, Err(ReplayError::Invalid(_))), "{first:?}"),
            }
            // The expired replay is gone, even asked about at the arrival
            // time — not held with its cursor moved on, as it would be had
            // either call been served.
            assert!(
                matches!(
                    registry.status(store, &id, Some(""), arrival),
                    Err(ReplayError::NotFound)
                ),
                "{case}: the expired replay is still held"
            );
        }
    }

    // ------------------------------------------------------------- forks

    fn fork_spec(edits: Value, live_after: Value) -> ForkSpec {
        ForkSpec::parse(Some(&edits), Some(&live_after)).expect("parses")
    }

    fn render_openai(step: &ServedStep) -> Result<Value, ReplayError> {
        let rendered = step.openai(&json!({}))?;
        Ok(serde_json::from_slice(&rendered.body).expect("json"))
    }

    #[test]
    fn a_fork_serves_its_edit_matched_strictly_up_to_it_and_loosely_after() {
        let fixture = TempStore::open("fork-edit", crate::Config::default());
        let store = fixture.store();
        openllmetry_run(store);
        let registry = Registry::default();
        let spec = fork_spec(
            json!([{"step": 0, "response": {"content": "It is sunny in Paris."}}]),
            Value::Null,
        );
        let created = registry
            .create_fork(store, Some(""), "run-1", Mode::Strict, &spec, 1 << 20, NOW)
            .expect("creates");
        let fork = created.fork.clone().expect("a fork");
        assert!(fork.id.starts_with("fk_") && fork.id.len() == 35);
        assert_eq!((fork.parent.as_str(), fork.step), ("run-1", 0));
        let body = created.to_json();
        assert_eq!(body["fork_id"], json!(fork.id));
        assert_eq!(
            body["fork"]["attributes"],
            json!({"traza.fork.parent": "run-1", "traza.fork.step": 0, "traza.fork.id": fork.id})
        );

        // Step 0 is the edited step: the agent still asks the recorded
        // question there, so a different one is refused.
        let mut wrong = first_request();
        wrong["messages"][1]["content"] = json!("Weather in Rome?");
        assert!(matches!(
            registry.next_llm(
                store,
                &created.id,
                Some(""),
                openai_request_digest(&wrong).expect("parses"),
                NOW,
                render_openai
            ),
            Err(ReplayError::Diverged { step: 0, .. })
        ));
        let answer = registry
            .next_llm(
                store,
                &created.id,
                Some(""),
                openai_request_digest(&first_request()).expect("parses"),
                NOW,
                render_openai,
            )
            .expect("serves the edit");
        assert_eq!(
            answer["choices"][0]["message"]["content"],
            json!("It is sunny in Paris.")
        );
        assert_eq!(answer["choices"][0]["finish_reason"], json!("stop"));
        assert!(
            answer["id"].as_str().unwrap().starts_with("chatcmpl-traza"),
            "an edited answer does not reuse the recorded answer's id: {answer}"
        );
        assert_eq!(answer["usage"]["total_tokens"], json!(0));
        assert_eq!(answer["model"], json!("gpt-4o-2024-08-06"));

        // After the edit the agent is on a new branch: whatever it asks, the
        // next recorded step is served, and the match is only recorded.
        let served = registry
            .next_llm(
                store,
                &created.id,
                Some(""),
                Some("f".repeat(64)),
                NOW,
                keep_step,
            )
            .expect("loose after the edit");
        assert_eq!(served.step, 2);
        let status = registry
            .status(store, &created.id, Some(""), NOW)
            .expect("live");
        assert_eq!(status["fork_id"], json!(fork.id));
        assert_eq!(
            status["fork"]["edits"],
            json!([{"step": 0, "kind": "response", "served": true}])
        );
        let matched: Vec<bool> = status["calls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|call| call["matched"].as_bool().unwrap())
            .collect();
        assert_eq!(matched, [false, true, false]);
        assert_eq!(status["diverged"]["step"], json!(0));
    }

    #[test]
    fn a_live_point_answers_replay_live_and_holds_nothing_past_it() {
        let fixture = TempStore::open("fork-live", crate::Config::default());
        let store = fixture.store();
        openllmetry_run(store);
        // A third model call recorded with content capture off: unreplayable,
        // but past the live point nobody needs it.
        store
            .ingest_batch(vec![span(
                "t1",
                "d-blind",
                400,
                json!({"session.id": "run-1", "gen_ai.operation.name": "chat",
                       "gen_ai.request.model": "gpt-4o"}),
            )])
            .expect("ingests");
        let registry = Registry::default();
        assert!(matches!(
            registry.create(store, Some(""), "run-1", Mode::Strict, 1 << 20, NOW),
            Err(ReplayError::Unreplayable(_))
        ));
        let spec = fork_spec(json!([]), json!(0));
        let created = registry
            .create_fork(store, Some(""), "run-1", Mode::Strict, &spec, 1 << 20, NOW)
            .expect("the steps past the live point are not held");
        assert_eq!(created.llm_steps, 1);
        assert_eq!(created.fork.as_ref().map(|fork| fork.step), Some(1));
        registry
            .next_llm(
                store,
                &created.id,
                Some(""),
                openai_request_digest(&first_request()).expect("parses"),
                NOW,
                keep_step,
            )
            .expect("step 0 is replayed");
        for _ in 0..2 {
            match registry.next_llm(store, &created.id, Some(""), None, NOW + 7, keep_step) {
                Err(ReplayError::Live {
                    live_after,
                    served,
                    fork_id,
                }) => {
                    assert_eq!((live_after, served), (0, 1));
                    assert_eq!(fork_id, created.fork.as_ref().map(|fork| fork.id.clone()));
                }
                other => panic!("expected the live point, got {other:?}"),
            }
        }
        // The tool step after the live point is not held either.
        assert!(matches!(
            registry.next_tool(store, &created.id, Some(""), Some("get_weather"), NOW),
            Err(ReplayError::Live { .. })
        ));
        let status = registry
            .status(store, &created.id, Some(""), NOW + 9)
            .expect("live");
        // Judged at the call's arrival plus the time it spent getting there.
        let live_at = status["fork"]["live_at_ns"].as_u64().expect("went live");
        assert!(
            (NOW + 7..NOW + 7 + 60_000_000_000).contains(&live_at),
            "{live_at}"
        );
        assert_eq!(status["fork"]["live_calls"], json!(3));
        assert_eq!(
            status["exhausted_calls"],
            json!(0),
            "the live point is not exhaustion"
        );
        assert_eq!(status["remaining"], json!(0));
    }

    #[test]
    fn a_tool_result_edit_is_served_by_tools_next_and_ends_strict_matching() {
        let fixture = TempStore::open("fork-tool", crate::Config::default());
        let store = fixture.store();
        openllmetry_run(store);
        let registry = Registry::default();
        let spec = fork_spec(json!([{"step": 1, "tool_result": "sunny, 25C"}]), json!(2));
        let created = registry
            .create_fork(store, Some(""), "run-1", Mode::Strict, &spec, 1 << 20, NOW)
            .expect("creates");
        assert_eq!(created.fork.as_ref().map(|fork| fork.step), Some(1));
        let tool = registry
            .next_tool(store, &created.id, Some(""), Some("get_weather"), NOW)
            .expect("serves the edit");
        assert_eq!(
            tool.to_json(),
            json!({"step": 1, "tool": "get_weather", "arguments": {"city": "Paris"},
                   "result": "sunny, 25C", "edited": true})
        );
        registry
            .next_llm(
                store,
                &created.id,
                Some(""),
                openai_request_digest(&first_request()).expect("parses"),
                NOW,
                keep_step,
            )
            .expect("step 0 is before the edit, and matches");
        // The agent now sends the edited tool result back: not the recorded
        // request, and served all the same.
        let mut edited = second_request();
        edited["messages"][3]["content"] = json!("sunny, 25C");
        let served = registry
            .next_llm(
                store,
                &created.id,
                Some(""),
                openai_request_digest(&edited).expect("parses"),
                NOW,
                keep_step,
            )
            .expect("strict matching ends at the first edit");
        assert_eq!(served.step, 2);
        let status = registry
            .status(store, &created.id, Some(""), NOW)
            .expect("live");
        assert_eq!(
            status["fork"]["edits"],
            json!([{"step": 1, "kind": "tool_result", "served": true}])
        );
        assert_eq!(status["diverged"], Value::Null);
    }

    #[test]
    fn a_fork_the_run_cannot_take_is_invalid_before_and_after_the_read() {
        let fixture = TempStore::open("fork-invalid", crate::Config::default());
        let store = fixture.store();
        openllmetry_run(store);
        let registry = Registry::default();
        let refused = |edits: Value, live: Value| match registry.create_fork(
            store,
            Some(""),
            "run-1",
            Mode::Strict,
            &fork_spec(edits, live),
            1 << 20,
            NOW,
        ) {
            Err(ReplayError::Invalid(message)) => message,
            other => panic!("expected an invalid fork, got {other:?}"),
        };
        assert!(refused(json!([{"step": 1, "response": "hi"}]), Value::Null)
            .contains("step 1 is a tool call"));
        assert!(
            refused(json!([{"step": 0, "tool_result": "x"}]), Value::Null)
                .contains("step 0 is a model call")
        );
        assert!(
            refused(json!([{"step": 0, "response": {"count": 2}}]), Value::Null)
                .contains("the edit at step 0")
        );
        assert!(refused(
            json!([{"step": 0, "response": [{"role": "assistant"}]}]),
            Value::Null
        )
        .contains("answer text or a tool call"));
        assert!(refused(json!([]), json!(3)).contains("live_after 3 is past"));
        assert_eq!(registry.len(NOW), 0, "nothing was registered");
    }

    #[test]
    fn a_divergence_is_explained_where_the_request_first_differs() {
        let fixture = TempStore::open("fork-explain", crate::Config::default());
        let store = fixture.store();
        openllmetry_run(store);
        let registry = Registry::default();
        let created = registry
            .create(store, Some(""), "run-1", Mode::Strict, 1 << 20, NOW)
            .expect("creates");
        let mut wrong = first_request();
        wrong["messages"][1]["content"] = json!("Weather in Rome?");
        let got = openai_request_digest(&wrong).expect("parses");
        assert!(registry
            .next_llm(store, &created.id, Some(""), got, NOW, keep_step)
            .is_err());
        let difference = registry
            .explain_divergence(
                store,
                &created.id,
                Some(""),
                0,
                &wrong,
                Provider::OpenAi,
                NOW,
            )
            .expect("explained");
        assert_eq!(
            difference,
            json!({"message": 1, "role": "user", "field": "content", "at_char": 11,
                   "recorded": "Weather in Paris?", "got": "Weather in Rome?",
                   "recorded_messages": 2, "got_messages": 2})
        );
        let status = registry
            .status(store, &created.id, Some(""), NOW)
            .expect("live");
        assert_eq!(status["diverged"]["difference"], difference);

        // A foreign tenant learns nothing.
        assert!(registry
            .explain_divergence(
                store,
                &created.id,
                Some("acme"),
                0,
                &wrong,
                Provider::OpenAi,
                NOW
            )
            .is_none());
    }

    #[test]
    fn a_difference_names_missing_extra_and_long_fields() {
        let message = |role: &str, content: &str| Normalised {
            role: Some(json!(role)),
            content: Some(json!(content)),
            ..Normalised::default()
        };
        let recorded = vec![message("system", "Be brief."), message("user", "hi")];
        let shorter = describe_difference(&recorded, &recorded[..1]).expect("differs");
        assert_eq!(shorter["field"], json!("missing"));
        assert_eq!(shorter["recorded"], json!("hi"));
        assert_eq!(shorter["got"], Value::Null);
        let mut longer = recorded.clone();
        longer.push(message("user", "and more"));
        let extra = describe_difference(&recorded, &longer).expect("differs");
        assert_eq!(
            (extra["field"].clone(), extra["message"].clone()),
            (json!("extra"), json!(2))
        );
        assert!(describe_difference(&recorded, &recorded).is_none());

        // A long field is quoted from just before the difference, marked.
        let long = "x".repeat(300);
        let changed = format!("{}y{}", &long[..200], &long[201..]);
        let quoted = describe_difference(&[message("user", &long)], &[message("user", &changed)])
            .expect("differs");
        assert_eq!(quoted["at_char"], json!(200));
        let recorded_quote = quoted["recorded"].as_str().unwrap();
        assert!(recorded_quote.starts_with('…') && recorded_quote.ends_with('…'));
        assert_eq!(recorded_quote.chars().count(), DIFFERENCE_CHARS + 2);
        // JSON-shaped text compares as JSON, exactly as strict matching does.
        let spaced = message("tool", "{\"a\": 1}");
        let compact = message("tool", "{\"a\":1}");
        assert!(describe_difference(&[spaced], &[compact]).is_none());
    }

    #[test]
    fn calls_past_the_recording_are_counted() {
        let fixture = TempStore::open("replay-extra", crate::Config::default());
        let store = fixture.store();
        openllmetry_run(store);
        let registry = Registry::default();
        let id = registry
            .create(store, Some(""), "run-1", Mode::Loose, 1 << 20, NOW)
            .expect("creates")
            .id;
        for _ in 0..2 {
            registry
                .next_llm(store, &id, Some(""), None, NOW, keep_step)
                .expect("serves");
        }
        for _ in 0..3 {
            assert!(matches!(
                registry.next_llm(store, &id, Some(""), None, NOW, keep_step),
                Err(ReplayError::Exhausted { served: 2 })
            ));
        }
        let status = registry.status(store, &id, Some(""), NOW).expect("live");
        assert_eq!(status["exhausted_calls"], json!(3));
        assert!(status.get("fork").is_none(), "a plain replay is no fork");
    }
}
