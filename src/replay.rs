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
//!   divergence rather than silently mismatching answers to questions.
//!
//! The registry is bounded — [`MAX_LIVE_REPLAYS`] replays, each living
//! [`REPLAY_TTL`] from creation, holding at most [`MAX_REGISTRY_BYTES`] of
//! recorded content between them — and lives in memory only: a restart forgets
//! every replay, and creating one again is a read of the store.

use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::semconv::{self, StepKind};
use crate::tape::{self, TapeOptions, TapeStep};
use crate::{Span, Store};

/// How long a replay lives, counted from creation. Fixed rather than renewed
/// by use, so when a replay expires is a fact of its creation response.
pub const REPLAY_TTL: Duration = Duration::from_secs(60 * 60);

/// Replays held at once. Past it, creation is refused until one expires —
/// never by evicting a live replay, which would break somebody's run halfway.
pub const MAX_LIVE_REPLAYS: usize = 1024;

/// Recorded content the registry holds across every live replay, measured as
/// the serialized size of the answers and tool records each one snapshots.
///
/// The count bound alone is not a memory bound: one replay of a long run with
/// large completions can be thousands of times another. Half a gibibyte is
/// generous for replaying agent runs and still a number an operator can plan
/// a process around.
pub const MAX_REGISTRY_BYTES: usize = 512 * 1024 * 1024;

/// Calls one replay records for `GET /v1/replays/{id}`. A client stuck
/// retrying a divergence must not grow a replay without bound; past this the
/// log stops growing and serving continues.
const MAX_RECORDED_CALLS: usize = 4096;

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

/// Why a replay operation was refused.
#[derive(Debug)]
pub enum ReplayError {
    /// No live replay has that id — or it belongs to a tenant the caller is
    /// not bound to, which answers the same way: whether another tenant's
    /// replay exists is not this caller's to learn.
    NotFound,
    /// The run has no spans under the caller's tenant scope.
    RunNotFound,
    /// The registry is at [`MAX_LIVE_REPLAYS`] or [`MAX_REGISTRY_BYTES`].
    Full {
        /// Seconds until the oldest live replay expires and frees room.
        retry_after_seconds: u64,
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
    /// The store refused the read the replay needed.
    Engine(crate::Error),
}

impl From<crate::Error> for ReplayError {
    fn from(error: crate::Error) -> Self {
        Self::Engine(error)
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
}

impl Created {
    /// The `201` body of `POST /v1/replays`.
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "run": self.run,
            "steps": self.steps,
            "llm_steps": self.llm_steps,
            "mode": self.mode.as_str(),
            "expires_at_ns": self.expires_at_ns,
            "base_urls": {
                "openai": format!("/v1/replays/{}/openai/v1", self.id),
                "anthropic": format!("/v1/replays/{}/anthropic", self.id),
            },
        })
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
    /// The recorded result, `null` when none was captured.
    pub result: Value,
}

impl ServedTool {
    /// The `tools/next` response body.
    pub fn to_json(&self) -> Value {
        json!({
            "step": self.step,
            "tool": self.tool,
            "arguments": self.arguments,
            "result": self.result,
        })
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
}

/// A recorded tool step inside a live replay.
#[derive(Clone, Debug)]
struct ToolStep {
    step: usize,
    tool: Option<String>,
    arguments: Value,
    result: Value,
    served: bool,
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
}

struct Replay {
    run: String,
    /// The tenant whose data the replay holds: `Some` when it was created
    /// under a tenant scope, `None` for the operator view.
    tenant: Option<String>,
    mode: Mode,
    expires_at_ns: u64,
    llm: Vec<LlmStep>,
    tools: Vec<ToolStep>,
    served: usize,
    diverged: Option<Divergence>,
    calls: Vec<CallRecord>,
    bytes: usize,
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
}

/// The in-memory registry of live replays.
pub struct Registry {
    replays: Mutex<HashMap<String, Replay>>,
    ttl: Duration,
    max_live: usize,
    max_bytes: usize,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new(REPLAY_TTL, MAX_LIVE_REPLAYS, MAX_REGISTRY_BYTES)
    }
}

impl Registry {
    /// A registry with explicit bounds; [`Registry::default`] uses the
    /// documented ones.
    pub fn new(ttl: Duration, max_live: usize, max_bytes: usize) -> Self {
        Self {
            replays: Mutex::new(HashMap::new()),
            ttl,
            max_live,
            max_bytes,
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

    /// Snapshots `run_id`'s tape under `tenant` and registers a replay of it.
    ///
    /// `tape_max_bytes` bounds the payload bytes resolved while snapshotting
    /// (`--tape-max-bytes`). Refused, naming the step, when a model step
    /// recorded no answer (content capture was off), when an answer or — in
    /// strict mode — a request is an offloaded payload that could not be read,
    /// or when strict mode has no recorded message list to compare against.
    pub fn create(
        &self,
        store: &Store,
        tenant: Option<&str>,
        run_id: &str,
        mode: Mode,
        tape_max_bytes: usize,
        now_ns: u64,
    ) -> Result<Created, ReplayError> {
        // Cheap refusal first: a full registry should not cost a store read.
        self.check_room(now_ns, 0)?;
        let tape = store
            .tape(
                tenant,
                run_id,
                TapeOptions {
                    max_steps: tape::MAX_TAPE_SPANS,
                    resolve: Some(tape_max_bytes),
                },
            )?
            .ok_or(ReplayError::RunNotFound)?;
        let (llm, tools) = snapshot(&tape.steps, mode)?;
        let bytes = llm
            .iter()
            .map(|step| approximate_bytes(&step.choices))
            .chain(
                tools
                    .iter()
                    .map(|tool| value_bytes(&tool.arguments) + value_bytes(&tool.result) + 64),
            )
            .sum::<usize>();

        let mut replays = self.lock();
        self.purge(&mut replays, now_ns);
        self.ensure_room(&replays, now_ns, bytes)?;
        let id = new_id();
        let ttl_ns = self.ttl.as_nanos().min(u128::from(u64::MAX)) as u64;
        let created = Created {
            id: id.clone(),
            run: run_id.to_owned(),
            steps: tape.steps.len(),
            llm_steps: llm.len(),
            mode,
            expires_at_ns: now_ns.saturating_add(ttl_ns),
        };
        replays.insert(
            id,
            Replay {
                run: run_id.to_owned(),
                tenant: tenant.map(str::to_owned),
                mode,
                expires_at_ns: created.expires_at_ns,
                llm,
                tools,
                served: 0,
                diverged: None,
                calls: Vec::new(),
                bytes,
            },
        );
        Ok(created)
    }

    fn check_room(&self, now_ns: u64, bytes: usize) -> Result<(), ReplayError> {
        let mut replays = self.lock();
        self.purge(&mut replays, now_ns);
        self.ensure_room(&replays, now_ns, bytes)
    }

    fn ensure_room(
        &self,
        replays: &HashMap<String, Replay>,
        now_ns: u64,
        bytes: usize,
    ) -> Result<(), ReplayError> {
        let held: usize = replays.values().map(|replay| replay.bytes).sum();
        if replays.len() < self.max_live && held.saturating_add(bytes) <= self.max_bytes {
            return Ok(());
        }
        let oldest = replays
            .values()
            .map(|replay| replay.expires_at_ns)
            .min()
            .unwrap_or(now_ns);
        // Rounded up: a retry that arrives a fraction early would be refused
        // again for nothing.
        let retry_after_seconds = oldest.saturating_sub(now_ns).div_ceil(1_000_000_000).max(1);
        Err(ReplayError::Full {
            retry_after_seconds,
        })
    }

    fn purge(&self, replays: &mut HashMap<String, Replay>, now_ns: u64) {
        replays.retain(|_, replay| replay.expires_at_ns > now_ns);
    }

    /// Serves the next recorded llm step to a request whose messages
    /// normalise to `got_digest` ([`openai_request_digest`],
    /// [`anthropic_request_digest`]).
    pub fn next_llm(
        &self,
        id: &str,
        binding: Option<&str>,
        got_digest: Option<String>,
        now_ns: u64,
    ) -> Result<ServedStep, ReplayError> {
        let mut replays = self.lock();
        self.purge(&mut replays, now_ns);
        let replay = replays
            .get_mut(id)
            .filter(|replay| replay.visible_to(binding))
            .ok_or(ReplayError::NotFound)?;
        let Some(step) = replay.llm.get(replay.served).cloned() else {
            return Err(ReplayError::Exhausted {
                served: replay.served,
            });
        };
        let matched = step.expected_digest.is_some() && step.expected_digest == got_digest;
        if replay.mode == Mode::Strict && !matched {
            replay.record(step.step, false, now_ns);
            if replay.diverged.is_none() {
                replay.diverged = Some(Divergence {
                    step: step.step,
                    expected_digest: step.expected_digest.clone(),
                    got_digest: got_digest.clone(),
                });
            }
            return Err(ReplayError::Diverged {
                step: step.step,
                expected_digest: step.expected_digest,
                got_digest,
            });
        }
        replay.record(step.step, matched, now_ns);
        replay.served += 1;
        Ok(ServedStep {
            replay: id.to_owned(),
            step: step.step,
            model: step.model,
            response_id: step.response_id,
            start_ns: step.start_ns,
            choices: step.choices,
            tokens: step.tokens,
        })
    }

    /// Serves the next unserved recorded tool step, or the next one named
    /// `tool`. Tool steps have their own cursor, independent of the llm one,
    /// so a harness may take results whenever its agent calls a tool.
    pub fn next_tool(
        &self,
        id: &str,
        binding: Option<&str>,
        tool: Option<&str>,
        now_ns: u64,
    ) -> Result<ServedTool, ReplayError> {
        let mut replays = self.lock();
        self.purge(&mut replays, now_ns);
        let replay = replays
            .get_mut(id)
            .filter(|replay| replay.visible_to(binding))
            .ok_or(ReplayError::NotFound)?;
        let next = replay.tools.iter_mut().find(|candidate| {
            !candidate.served && tool.map_or(true, |name| candidate.tool.as_deref() == Some(name))
        });
        let Some(next) = next else {
            return Err(ReplayError::ToolsExhausted {
                tool: tool.map(str::to_owned),
            });
        };
        next.served = true;
        Ok(ServedTool {
            step: next.step,
            tool: next.tool.clone(),
            arguments: next.arguments.clone(),
            result: next.result.clone(),
        })
    }

    /// The status document of `GET /v1/replays/{id}`.
    pub fn status(
        &self,
        id: &str,
        binding: Option<&str>,
        now_ns: u64,
    ) -> Result<Value, ReplayError> {
        let mut replays = self.lock();
        self.purge(&mut replays, now_ns);
        let replay = replays
            .get(id)
            .filter(|replay| replay.visible_to(binding))
            .ok_or(ReplayError::NotFound)?;
        let calls: Vec<Value> = replay
            .calls
            .iter()
            .map(|call| json!({"step": call.step, "matched": call.matched, "at_ns": call.at_ns}))
            .collect();
        Ok(json!({
            "id": id,
            "run": replay.run,
            "mode": replay.mode.as_str(),
            "served": replay.served,
            "remaining": replay.llm.len() - replay.served,
            "diverged": replay.diverged.as_ref().map(|divergence| json!({
                "step": divergence.step,
                "expected_digest": divergence.expected_digest,
                "got_digest": divergence.got_digest,
            })),
            "calls": calls,
            "expires_at_ns": replay.expires_at_ns,
        }))
    }

    /// Live replays, for tests and diagnostics.
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

/// Builds a replay's llm and tool snapshots from a resolved tape, refusing
/// what cannot be replayed.
fn snapshot(steps: &[TapeStep], mode: Mode) -> Result<(Vec<LlmStep>, Vec<ToolStep>), ReplayError> {
    let mut llm = Vec::new();
    let mut tools = Vec::new();
    for step in steps {
        match step.kind {
            StepKind::Llm => llm.push(llm_snapshot(step, mode)?),
            StepKind::Tool => tools.push(ToolStep {
                step: step.index,
                tool: step.tool.clone(),
                arguments: step.request.clone().unwrap_or(Value::Null),
                result: step.response.clone().unwrap_or(Value::Null),
                served: false,
            }),
            _ => {}
        }
    }
    Ok((llm, tools))
}

fn llm_snapshot(step: &TapeStep, mode: Mode) -> Result<LlmStep, ReplayError> {
    let label = || format!("step {} ({})", step.index, step.detail.signature);
    let Some(response) = &step.response else {
        return Err(ReplayError::Unreplayable(format!(
            "{} recorded no response — content capture was off when the run was recorded \
             (for OpenLLMetry, TRACELOOP_TRACE_CONTENT), so there is no answer to replay",
            label()
        )));
    };
    if tape::contains_reference(response) {
        return Err(ReplayError::Unreplayable(format!(
            "{}'s response is an offloaded payload that could not be read — erased, swept by \
             retention, or past --tape-max-bytes",
            label()
        )));
    }
    let choices = recorded_choices(response, &step.detail.finish_reasons);
    if choices.is_empty() {
        return Err(ReplayError::Unreplayable(format!(
            "{}'s response holds no assistant message to replay",
            label()
        )));
    }
    let expected_digest = match &step.request {
        Some(request) if !tape::contains_reference(request) => recorded_digest(request),
        _ => None,
    };
    if mode == Mode::Strict && expected_digest.is_none() {
        let why = match &step.request {
            None => "recorded no request".to_owned(),
            Some(request) if tape::contains_reference(request) => {
                "has a request that is an offloaded payload that could not be read".to_owned()
            }
            Some(_) => "recorded its request as text rather than a message list".to_owned(),
        };
        return Err(ReplayError::Unreplayable(format!(
            "{} {why}, so strict mode has nothing to compare an incoming request against; \
             create the replay with \"mode\": \"loose\" to serve steps in order",
            label()
        )));
    }
    Ok(LlmStep {
        step: step.index,
        model: step.model.clone(),
        response_id: step.detail.response_id.clone(),
        start_ns: step.start_ns,
        expected_digest,
        choices,
        tokens: step.tokens.clone(),
    })
}

fn value_bytes(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(0, |bytes| bytes.len())
}

fn approximate_bytes(choices: &[Choice]) -> usize {
    choices
        .iter()
        .map(|choice| {
            choice.text.as_ref().map_or(0, String::len)
                + choice
                    .calls
                    .iter()
                    .map(|call| call.name.len() + value_bytes(&call.arguments) + 32)
                    .sum::<usize>()
                + 64
        })
        .sum::<usize>()
        + 256
}

/// A fresh replay id: `rp_` and 32 hex characters.
///
/// Drawn from the standard library's per-process hash keys, which are seeded
/// from the operating system's randomness, over a counter and the clock. Ids
/// are hard to guess, but they are not credentials: every call through a
/// replay still authenticates, and its tenant binding is checked.
fn new_id() -> String {
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
    format!("rp_{high:016x}{low:016x}")
}

// ------------------------------------------------------------ normalisation

/// One message in the normalised shape: the fields step facts reassemble from
/// an indexed layout.
#[derive(Default)]
struct Normalised {
    role: Option<Value>,
    content: Option<Value>,
    tool_call_id: Option<Value>,
    tool_calls: Vec<[Option<Value>; 3]>,
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
fn recorded_digest(request: &Value) -> Option<String> {
    let messages = request.as_array()?;
    let normalised = messages
        .iter()
        .filter(|message| message.is_object())
        .map(
            |message| match message.get("parts").and_then(Value::as_array) {
                Some(parts) if message.get("content").is_none() => parts_message(message, parts),
                _ => normalised_message(message),
            },
        )
        .collect();
    normalised_digest(normalised)
}

/// The normalised digest of an OpenAI chat-completions request body.
///
/// Each message contributes what OpenLLMetry records of it: `role`; `content`
/// when non-empty (a list of content parts as the list); `tool_call_id`; and
/// each tool call's `id`, `function.name` and `function.arguments`. Errs when
/// `messages` is not a list.
pub fn openai_request_digest(body: &Value) -> Result<Option<String>, String> {
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or("the request body must carry a \"messages\" array")?;
    Ok(normalised_digest(
        messages.iter().map(normalised_message).collect(),
    ))
}

/// The normalised digest of an Anthropic messages request body.
///
/// A `system` prompt becomes a leading `{"role": "system", "content": …}`
/// message, exactly as OpenLLMetry's Anthropic instrumentation records it,
/// followed by each message's `role` and `content` (a list of content blocks
/// as the list). Errs when `messages` is not a list.
pub fn anthropic_request_digest(body: &Value) -> Result<Option<String>, String> {
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
    Ok(normalised_digest(normalised))
}

// --------------------------------------------------------- recorded answers

/// The recorded answer as choices, whichever shape the instrumentation stored
/// it in: the normalised message list (indexed layouts, native events), OTel
/// `parts` messages, a raw provider response object (OpenAI `choices`,
/// Anthropic `content` blocks), or plain text.
fn recorded_choices(response: &Value, finish_reasons: &[String]) -> Vec<Choice> {
    let mut choices = match response {
        Value::Array(items) if items.iter().all(is_message) && !items.is_empty() => items
            .iter()
            .filter(|message| !is_reasoning(message))
            .map(message_choice)
            .collect(),
        Value::Object(object) => {
            if let Some(list) = object.get("choices").and_then(Value::as_array) {
                list.iter()
                    .map(|choice| {
                        let mut parsed =
                            message_choice(choice.get("message").unwrap_or(&Value::Null));
                        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                            parsed.finish_reason = Some(reason.to_owned());
                        }
                        parsed
                    })
                    .collect()
            } else if is_message(response) {
                vec![message_choice(response)]
            } else {
                vec![text_choice(json_text(response))]
            }
        }
        Value::String(text) => vec![text_choice(text.clone())],
        Value::Null => Vec::new(),
        other => vec![text_choice(json_text(other))],
    };
    for (index, choice) in choices.iter_mut().enumerate() {
        if choice.finish_reason.is_none() {
            choice.finish_reason = finish_reasons.get(index).cloned();
        }
    }
    choices
}

/// Whether a recorded value is a message rather than JSON text an answer
/// happened to be. A completion that read `[1, 2]` is parsed as JSON by the
/// value rule, and must still be served as the text it was.
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

/// JSON-shaped text, written back as text: compact, keys in sorted order.
fn json_text(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
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

impl ServedStep {
    fn model_or(&self, request: &Value) -> String {
        self.model
            .clone()
            .or_else(|| {
                request
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "unknown".to_owned())
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
    /// `data: [DONE]`.
    pub fn openai(&self, request: &Value) -> Rendered {
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
            return Rendered {
                content_type: "application/json",
                body: serde_json::to_vec(&body).unwrap_or_default(),
            };
        }

        let include_usage = request
            .pointer("/stream_options/include_usage")
            .and_then(Value::as_bool)
            == Some(true);
        let mut out = String::new();
        let mut chunk = |choices: Value, usage_field: Option<Value>| {
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
            out.push_str("data: ");
            out.push_str(&serde_json::to_string(&event).unwrap_or_default());
            out.push_str("\n\n");
        };
        for (index, choice) in self.choices.iter().enumerate() {
            let delta_choice = |delta: Value, finish: Value| json!([{"index": index, "delta": delta, "logprobs": null, "finish_reason": finish}]);
            let opening = if choice.calls.is_empty() || choice.text.is_some() {
                json!({"role": "assistant", "content": "", "refusal": null})
            } else {
                json!({"role": "assistant", "content": null, "refusal": null})
            };
            chunk(delta_choice(opening, Value::Null), None);
            if let Some(text) = &choice.text {
                for piece in deltas(text) {
                    chunk(delta_choice(json!({"content": piece}), Value::Null), None);
                }
            }
            for (position, call) in choice.calls.iter().enumerate() {
                chunk(
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
                );
                for piece in deltas(&openai_arguments(&call.arguments)) {
                    chunk(
                        delta_choice(
                            json!({"tool_calls": [{
                                "index": position,
                                "function": {"arguments": piece},
                            }]}),
                            Value::Null,
                        ),
                        None,
                    );
                }
            }
            chunk(delta_choice(json!({}), json!(openai_finish(choice))), None);
        }
        if include_usage {
            chunk(json!([]), Some(usage));
        }
        out.push_str("data: [DONE]\n\n");
        Rendered {
            content_type: "text/event-stream",
            body: out.into_bytes(),
        }
    }

    /// The step as an Anthropic `Message` — or, when the request asked to
    /// stream, as the `message_start` / `content_block_*` / `message_delta` /
    /// `message_stop` event sequence.
    ///
    /// Anthropic answers with one message, so a recording with several
    /// assistant messages is served as their texts concatenated and their
    /// tool calls in order.
    pub fn anthropic(&self, request: &Value) -> Rendered {
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
            return Rendered {
                content_type: "application/json",
                body: serde_json::to_vec(&body).unwrap_or_default(),
            };
        }

        let mut out = String::new();
        let mut event = |name: &str, data: Value| {
            out.push_str("event: ");
            out.push_str(name);
            out.push_str("\ndata: ");
            out.push_str(&serde_json::to_string(&data).unwrap_or_default());
            out.push_str("\n\n");
        };
        let mut start_usage = usage.clone();
        start_usage["output_tokens"] = json!(0);
        event(
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
        );
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
                "content_block_start",
                json!({"type": "content_block_start", "index": index, "content_block": opening}),
            );
            for piece in pieces {
                let mut delta = Map::new();
                delta.insert("type".to_owned(), json!(delta_type));
                delta.insert(field.to_owned(), json!(piece));
                event(
                    "content_block_delta",
                    json!({"type": "content_block_delta", "index": index, "delta": delta}),
                );
            }
            event(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": index}),
            );
        }
        event(
            "message_delta",
            json!({"type": "message_delta",
                   "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                   "usage": {"output_tokens": self.tokens.output.unwrap_or(0)}}),
        );
        event("message_stop", json!({"type": "message_stop"}));
        Rendered {
            content_type: "text/event-stream",
            body: out.into_bytes(),
        }
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

    const NOW: u64 = 1_800_000_000_000_000_000;

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
        match registry.next_llm(&created.id, Some(""), got.clone(), NOW + 1) {
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
                &created.id,
                Some(""),
                openai_request_digest(&first_request()).expect("parses"),
                NOW + 2,
            )
            .expect("matches");
        assert_eq!(served.step, 0);
        let body: Value = serde_json::from_slice(&served.openai(&first_request()).body).unwrap();
        assert_eq!(body["id"], json!("chatcmpl-rec1"));
        assert_eq!(body["model"], json!("gpt-4o-2024-08-06"));
        assert_eq!(body["choices"][0]["finish_reason"], json!("tool_calls"));
        let call = &body["choices"][0]["message"]["tool_calls"][0];
        assert_eq!(call["id"], json!("call_abc"));
        assert_eq!(call["function"]["arguments"], json!("{\"city\":\"Paris\"}"));
        assert_eq!(body["usage"]["total_tokens"], json!(52));

        let served = registry
            .next_llm(
                &created.id,
                Some(""),
                openai_request_digest(&second_request()).expect("parses"),
                NOW + 3,
            )
            .expect("matches");
        assert_eq!(
            served.step, 2,
            "the tool step in between is not an llm step"
        );

        match registry.next_llm(&created.id, Some(""), None, NOW + 4) {
            Err(ReplayError::Exhausted { served }) => assert_eq!(served, 2),
            other => panic!("expected exhaustion, got {other:?}"),
        }

        let status = registry
            .status(&created.id, Some(""), NOW + 5)
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
            .next_llm(&created.id, None, Some("0".repeat(64)), NOW)
            .expect("loose serves anyway");
        assert_eq!(served.step, 0);
        let status = registry.status(&created.id, None, NOW).expect("live");
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
            registry.next_tool(&id, Some(""), Some("search"), NOW),
            Err(ReplayError::ToolsExhausted { tool: Some(_) })
        ));
        let tool = registry
            .next_tool(&id, Some(""), Some("get_weather"), NOW)
            .expect("serves");
        assert_eq!(
            tool.to_json(),
            json!({"step": 1, "tool": "get_weather",
                   "arguments": {"city": "Paris"}, "result": "rainy, 12C"})
        );
        assert!(matches!(
            registry.next_tool(&id, Some(""), None, NOW),
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
        let registry = Registry::new(Duration::from_secs(10), 2, MAX_REGISTRY_BYTES);
        let id = registry
            .create(store, Some("acme"), "trace:t7", Mode::Loose, 1 << 20, NOW)
            .expect("creates")
            .id;
        assert!(matches!(
            registry.status(&id, Some("beta"), NOW),
            Err(ReplayError::NotFound)
        ));
        assert!(registry.status(&id, Some("acme"), NOW).is_ok());
        assert!(
            registry.status(&id, None, NOW).is_ok(),
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
            registry.status(&operator, Some("acme"), NOW),
            Err(ReplayError::NotFound)
        ));
        // Full at two: refused, never evicting, with a retry hint.
        match registry.create(store, None, "trace:t7", Mode::Loose, 1 << 20, NOW) {
            Err(ReplayError::Full {
                retry_after_seconds,
            }) => assert_eq!(retry_after_seconds, 10),
            other => panic!("expected a full registry, got {other:?}"),
        }
        let later = NOW + 10_000_000_000;
        assert!(registry.is_empty(later), "both expired at their TTL");
        assert!(matches!(
            registry.status(&id, None, later),
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
            .next_llm(&loose.id, Some(""), None, NOW)
            .expect("serves");
        let body: Value = serde_json::from_slice(&served.anthropic(&json!({})).body).unwrap();
        assert_eq!(body["content"], json!([{"type": "text", "text": "hello"}]));
        assert_eq!(body["stop_reason"], json!("end_turn"));
    }

    #[test]
    fn recorded_answers_read_every_shape() {
        let reasons = ["length".to_owned()];
        // Parsed JSON text is served as the text it was.
        let json_answer = recorded_choices(&json!([1, 2]), &[]);
        assert_eq!(json_answer[0].text.as_deref(), Some("[1,2]"));
        // OTel parts.
        let parts = recorded_choices(
            &json!([{"role": "assistant", "parts": [
                {"type": "text", "content": "On it."},
                {"type": "tool_call", "id": "c9", "name": "f", "arguments": {"x": 1}},
            ], "finish_reason": "tool_call"}]),
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
            &[],
        );
        assert_eq!(anthropic[0].calls[0].arguments, json!({"q": "x"}));
        assert_eq!(
            anthropic_stop(anthropic[0].finish_reason.as_deref(), true),
            "tool_use"
        );
        // A finish reason recorded only as the span attribute fills in.
        let text = recorded_choices(&json!("done"), &reasons);
        assert_eq!(openai_finish(&text[0]), "length");
        // Reasoning messages are not part of the answer.
        let thinking = recorded_choices(
            &json!([{"role": "thinking", "content": "hmm"},
                    {"role": "assistant", "content": "yes"}]),
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
        };
        let request = json!({"stream": true, "stream_options": {"include_usage": true}});
        let openai = String::from_utf8(step.openai(&request).body).unwrap();
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

        let anthropic = String::from_utf8(step.anthropic(&json!({"stream": true})).body).unwrap();
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
}
