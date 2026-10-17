//! Tapes: a run's steps in order, with what went into each and what came out.
//!
//! A tape is the run read as a recording. Every span whose step kind is a
//! model call, a tool call or an agent invocation ([`StepKind::is_step`])
//! becomes one step, in the run's step order (`start, trace, span` — see
//! [`crate::run`]), carrying its request and response as stored, their
//! digests, its fingerprint, and its priced token usage.
//!
//! Three surfaces read runs this way and all three come through here, so they
//! cannot disagree about what a step is: `GET /v1/tapes/{run}`, the replay
//! proxy (which serves a tape's model answers back to an agent in place of the
//! model — see [`crate::replay`]), and run diffing ([`crate::diff`]).
//!
//! **Values are stored values.** A request or response that ingest offloaded
//! is its `{"$payload": "sha256/…"}` reference until a caller asks for
//! resolution, and resolution is bounded: inlining every payload of a long run
//! could otherwise turn one request into hundreds of megabytes. Digests and
//! fingerprints always describe the stored form, resolved or not, so a tape
//! fetched with and without `resolve` agrees on every identity it reports.

use std::collections::HashMap;
use std::time::Instant;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::semconv::{self, LlmFacts, StepFacts, StepKind};
use crate::{Deadline, Result, Span, Store};

/// Default ceiling on the payload bytes one tape inlines when resolution is
/// asked for (`--tape-max-bytes`).
///
/// Eight MiB is thirty-two payloads at the server's default 256 KiB offload
/// threshold — room for a long run's offloaded prompts — while keeping a
/// single tape response a size a browser or an SDK can hold without thought.
pub const DEFAULT_TAPE_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Payload content one [`Resolver`] keeps after reading it, so that a
/// payload referenced from many steps — an offloaded system prompt repeated
/// in every request — is read from disk once per request rather than once
/// per step. Past it, later payloads are read each time they are referenced.
const RESOLVE_CACHE_BYTES: usize = 8 * 1024 * 1024;

/// Spans one tape examines before it says it was truncated.
///
/// The same budget the session diagnosis uses: past it a run is analyzed from
/// its EARLIEST spans, because a tape is read from the start and a suffix would
/// be a recording with its beginning cut off.
pub const MAX_TAPE_SPANS: usize = 20_000;

/// Spans whose step facts are extracted between two deadline checks: few
/// enough that a run of large spans cannot outrun its budget by much, many
/// enough that the clock read is lost in the extraction work.
const DEADLINE_CHECK_SPANS: usize = 16;

/// Attribute keys whose values configure a model call rather than describe
/// it: a change in any of them between two runs is a CONFIG change, not a
/// different decision on the same input (see [`crate::diff`]).
///
/// Every `gen_ai.request.*` key qualifies (model, temperature, top_p,
/// max_tokens, seed, stop sequences, …) plus OpenInference's single
/// `llm.invocation_parameters` blob.
const CONFIG_PREFIX: &str = "gen_ai.request.";
const OI_INVOCATION_PARAMETERS: &str = "llm.invocation_parameters";

/// Token usage one step reported. Each count is `None` when the span did not
/// report it — a tool step reports none, and an unreported count is not zero.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct StepTokens {
    /// Prompt/input tokens.
    pub input: Option<u64>,
    /// Completion/output tokens.
    pub output: Option<u64>,
    /// Prompt tokens served from a provider's cache.
    pub cache_read: Option<u64>,
    /// Prompt tokens written into a provider's cache.
    pub cache_write: Option<u64>,
}

/// One step of a tape.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TapeStep {
    /// Position on the tape, from zero.
    pub index: usize,
    /// The span's trace.
    pub trace_id: String,
    /// The span.
    pub span_id: String,
    /// The span's parent, if it has one.
    pub parent_span_id: Option<String>,
    /// The step kind: `llm`, `tool` or `agent`.
    pub kind: StepKind,
    /// The span's operation name.
    pub name: String,
    /// The emitting service.
    pub service: String,
    /// The model, when the span names one ([`semconv::facts`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The tool, when the span names one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// The agent, when the span names one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Start time, Unix nanoseconds.
    pub start_ns: u64,
    /// `end - start`, saturating at zero.
    pub duration_ns: u64,
    /// The span's status as recorded.
    pub status: String,
    /// The step's input as stored, or resolved when the tape was asked to
    /// resolve payload references.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<Value>,
    /// The step's output, under the same rules as [`Self::request`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response: Option<Value>,
    /// Digest of the STORED input ([`semconv::step_facts`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_digest: Option<String>,
    /// Digest of the STORED output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_digest: Option<String>,
    /// `kind:name#hash` ([`StepFacts::fingerprint`]).
    pub fingerprint: String,
    /// Token usage as reported.
    pub tokens: StepTokens,
    /// Cost in USD, metered or priced by this store's table.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Facts the replay proxy and the differ need that the tape's wire shape
    /// does not carry.
    #[serde(skip)]
    pub(crate) detail: StepDetail,
}

/// The parts of a step only the replay proxy and the differ read.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct StepDetail {
    /// `kind:name` — the fingerprint without its request hash.
    pub(crate) signature: String,
    /// `gen_ai.tool.call.id`.
    pub(crate) tool_call_id: Option<String>,
    /// `gen_ai.response.id`, the provider's id for the answer.
    pub(crate) response_id: Option<String>,
    /// `gen_ai.response.finish_reasons`, in recorded order.
    pub(crate) finish_reasons: Vec<String>,
    /// The call's configuration attributes (see [`CONFIG_PREFIX`]); empty for
    /// steps that are not model calls.
    pub(crate) config: Map<String, Value>,
    /// The span's tenant — needed, with the session, to tell whether an
    /// erasure covers a copy of this step held outside the store.
    pub(crate) tenant: String,
    /// The span's session id under the store's session precedence, which is
    /// what a session erasure matches.
    pub(crate) session: Option<String>,
    /// An llm request's carriers kept apart until resolution
    /// ([`semconv::RequestCarriers`]); [`Resolver::resolve_request`] combines
    /// them.
    pub(crate) request_carriers: Option<semconv::RequestCarriers>,
    /// Where an llm response was read from, which decides how a replay
    /// decodes it.
    pub(crate) response_source: semconv::ResponseSource,
    /// A tool response recorded as empty (`null` or `""`), which the
    /// response field cannot tell from one never captured.
    pub(crate) empty_response: Option<Value>,
}

/// A run read as a tape.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Tape {
    /// The run id asked for.
    pub run: String,
    /// The tenant scope it was read under: a tenant (`""` is the default
    /// tenant), or `None` for the operator view across every tenant.
    pub tenant: Option<String>,
    /// The steps, in step order.
    pub steps: Vec<TapeStep>,
    /// Whether the run held more spans than [`MAX_TAPE_SPANS`], or more steps
    /// than the caller's maximum, so that `steps` is a prefix of the run.
    pub truncated: bool,
    /// Spans read to build the tape, steps and non-steps alike.
    pub spans_examined: usize,
    /// Present only when resolution was asked for: whether any payload
    /// reference was left unresolved — because the byte budget ran out, or
    /// because its bytes are gone (erased or swept by retention).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_partial: Option<bool>,
}

/// What a tape read should include.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TapeOptions {
    /// Steps to return at most; past it the tape is truncated.
    pub max_steps: usize,
    /// `Some(bytes)` inlines `$payload` references in requests and responses,
    /// inlining at most `bytes` of payload content per tape; `None` returns
    /// stored values.
    pub resolve: Option<usize>,
}

impl Default for TapeOptions {
    fn default() -> Self {
        Self {
            max_steps: MAX_TAPE_SPANS,
            resolve: None,
        }
    }
}

/// A run's spans and the tape made of them, read together so every number
/// derived from either describes the same instant.
pub(crate) struct RunTape {
    /// The run's spans in step order (at most the span cap).
    pub(crate) spans: Vec<Span>,
    /// The steps among them, at most the step cap.
    pub(crate) steps: Vec<TapeStep>,
    /// Whether either cap cut the run short.
    pub(crate) truncated: bool,
    /// Whether the SPAN cap did: then `spans` is the run's beginning, and
    /// anything summarised over them — cost, errors, how the run ended —
    /// describes that beginning, not the run.
    pub(crate) spans_truncated: bool,
}

impl Store {
    /// The tape of `run_id` under `tenant` (the scoping rules of
    /// [`Store::get_run_in`]), or `None` when no span carries that run id.
    ///
    /// Budgeted by [`crate::Config::query_deadline`] — ONE budget for the
    /// whole request, payload resolution included. Resolution reads each
    /// reference under the same tenant ([`Resolver`]), so a bound caller can
    /// inline only bytes its own tenant's spans reference.
    pub fn tape(
        &self,
        tenant: Option<&str>,
        run_id: &str,
        options: TapeOptions,
    ) -> Result<Option<Tape>> {
        let deadline = self.query_deadline(Instant::now());
        let read = self.run_tape(tenant, run_id, MAX_TAPE_SPANS, options.max_steps, deadline)?;
        if read.spans.is_empty() {
            return Ok(None);
        }
        let mut steps = read.steps;
        let resolved_partial = match options.resolve {
            None => None,
            Some(max_bytes) => {
                let mut budget = ResolveBudget::new(max_bytes);
                let mut resolver = Resolver::new(self, tenant, deadline);
                for step in &mut steps {
                    resolver.resolve_request(step, &mut budget)?;
                    if let Some(response) = step.response.as_mut() {
                        resolver.resolve(response, &mut budget)?;
                    }
                }
                Some(budget.unresolved)
            }
        };
        Ok(Some(Tape {
            run: run_id.to_owned(),
            tenant: tenant.map(str::to_owned),
            steps,
            truncated: read.truncated,
            spans_examined: read.spans.len(),
            resolved_partial,
        }))
    }

    /// A run's spans plus its steps, under a caller-anchored budget so a
    /// request reading several runs spends one budget across all of them.
    pub(crate) fn run_tape(
        &self,
        tenant: Option<&str>,
        run_id: &str,
        max_spans: usize,
        max_steps: usize,
        deadline: Option<Deadline>,
    ) -> Result<RunTape> {
        let run = self.run_spans(tenant, run_id, max_spans, deadline)?;
        let mut steps = Vec::new();
        let spans_truncated = run.truncated;
        let mut truncated = spans_truncated;
        for (examined, span) in run.spans.iter().enumerate() {
            // Extraction reassembles and digests every value the read
            // returned — work proportional to the run's bytes, done after
            // the read's own checks — so it spends the same budget.
            if examined % DEADLINE_CHECK_SPANS == 0 {
                Deadline::check(deadline, 0)?;
            }
            let (facts, extras, llm) = self.step_facts_detailed(span);
            if !facts.kind.is_step() {
                continue;
            }
            if steps.len() == max_steps {
                truncated = true;
                break;
            }
            steps.push(tape_step(steps.len(), span, facts, extras, llm));
        }
        Ok(RunTape {
            spans: run.spans,
            steps,
            truncated,
            spans_truncated,
        })
    }
}

/// Payload bytes still available to one tape's resolution, and what it left
/// unresolved.
///
/// The budget is a PREFIX cut: the first reference that does not fit
/// exhausts it, and every reference after it stays a reference even if it
/// would fit. A tape is therefore "resolved up to a point", which a reader
/// can reason about, rather than resolved in a scatter that depends on the
/// sizes involved. Each occurrence is charged — a payload inlined at three
/// steps is three copies in the response.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ResolveBudget {
    limit: usize,
    remaining: usize,
    exhausted: bool,
    /// Whether any reference was left as a reference.
    pub(crate) unresolved: bool,
}

impl ResolveBudget {
    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            limit: max_bytes,
            remaining: max_bytes,
            exhausted: false,
            unresolved: false,
        }
    }

    /// Payload bytes inlined so far.
    pub(crate) fn spent(&self) -> usize {
        self.limit - self.remaining
    }
}

/// What one [`Resolver::resolve`] call left as references, and why.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Leftover {
    /// A reference stayed because the byte budget ran out at or before it.
    pub(crate) past_budget: bool,
    /// A reference stayed because its bytes cannot be read in scope: erased,
    /// swept by retention, withheld by a pending erasure, unreachable from
    /// the caller's tenant, or not text.
    pub(crate) unreadable: bool,
}

impl Leftover {
    /// Whether anything stayed a reference.
    pub(crate) fn any(self) -> bool {
        self.past_budget || self.unreadable
    }

    /// Folds in what another resolution left.
    pub(crate) fn merge(&mut self, other: Leftover) {
        self.past_budget |= other.past_budget;
        self.unreadable |= other.unreadable;
    }
}

/// Payload resolution for ONE request: its tenant scope, its compute budget,
/// and what it has already learned about each reference.
///
/// Every reference costs a deadline check before it is read, so resolving a
/// long run spends the request's one budget rather than a fresh budget per
/// reference — and a reference whose bytes are gone, which consumes no byte
/// budget, still consumes time and is still stopped by the deadline. What a
/// reference resolved to is remembered: its tenant-reachability probe (a
/// write-buffer walk plus a segment scan) runs once per request however many
/// steps repeat it, and its content is kept while [`RESOLVE_CACHE_BYTES`]
/// lasts.
pub(crate) struct Resolver<'s> {
    store: &'s Store,
    tenant: Option<&'s str>,
    deadline: Option<Deadline>,
    /// Whether each reference probed so far was readable in scope.
    readable: HashMap<String, bool>,
    /// Content already read: the value it resolves to, and its byte size.
    cache: HashMap<String, (Value, usize)>,
    cached_bytes: usize,
}

impl<'s> Resolver<'s> {
    /// A resolver reading under `tenant` (the scoping of
    /// [`Store::payload_in`]) within `deadline`.
    pub(crate) fn new(
        store: &'s Store,
        tenant: Option<&'s str>,
        deadline: Option<Deadline>,
    ) -> Self {
        Self {
            store,
            tenant,
            deadline,
            readable: HashMap::new(),
            cache: HashMap::new(),
            cached_bytes: 0,
        }
    }

    /// Replaces every `$payload` reference inside `value` with the content it
    /// names, while `budget` lasts, and reports what stayed a reference.
    ///
    /// Resolved text is read under the same rule as an inline value
    /// ([`semconv::field_value`]): text holding a JSON object or array becomes
    /// that JSON, so a resolved request has the shape it would have had if it
    /// had never been offloaded. Content that came out of a payload is not
    /// walked again: a producer's own text that happens to look like a
    /// reference is data, not an instruction to read another file.
    ///
    /// Errs when the request's compute budget runs out, or the store fails;
    /// a reference that cannot be read is a [`Leftover`], not an error.
    pub(crate) fn resolve(
        &mut self,
        value: &mut Value,
        budget: &mut ResolveBudget,
    ) -> Result<Leftover> {
        let mut leftover = Leftover::default();
        self.walk(value, budget, &mut leftover)?;
        Ok(leftover)
    }

    /// Resolves `step`'s request — through its carriers when it has them
    /// ([`StepDetail::request_carriers`]), which are resolved one by one
    /// (system instructions first, the order they appear in the stored
    /// request) and then combined by the same rule as inline carriers, so
    /// an offloaded message list ends up with its system instructions in
    /// front exactly as if it had never been offloaded.
    pub(crate) fn resolve_request(
        &mut self,
        step: &mut TapeStep,
        budget: &mut ResolveBudget,
    ) -> Result<Leftover> {
        let Some(semconv::RequestCarriers {
            mut messages,
            mut system,
        }) = step.detail.request_carriers.take()
        else {
            return match step.request.as_mut() {
                Some(request) => self.resolve(request, budget),
                None => Ok(Leftover::default()),
            };
        };
        let mut leftover = self.resolve(&mut system, budget)?;
        if let Some(messages) = messages.as_mut() {
            leftover.merge(self.resolve(messages, budget)?);
        }
        step.request = semconv::with_system_instructions(messages, Some(system));
        Ok(leftover)
    }

    fn walk(
        &mut self,
        value: &mut Value,
        budget: &mut ResolveBudget,
        leftover: &mut Leftover,
    ) -> Result<()> {
        if let Some(reference) = payload_reference(value) {
            if budget.exhausted {
                budget.unresolved = true;
                leftover.past_budget = true;
                return Ok(());
            }
            let reference = reference.to_owned();
            let Some((resolved, size)) = self.read(&reference)? else {
                budget.unresolved = true;
                leftover.unreadable = true;
                return Ok(());
            };
            if size > budget.remaining {
                budget.exhausted = true;
                budget.unresolved = true;
                leftover.past_budget = true;
                return Ok(());
            }
            budget.remaining -= size;
            *value = resolved;
            return Ok(());
        }
        match value {
            Value::Array(items) => {
                for item in items {
                    self.walk(item, budget, leftover)?;
                }
            }
            Value::Object(fields) => {
                for field in fields.values_mut() {
                    self.walk(field, budget, leftover)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// The value `reference` resolves to and its size in bytes, or `None`
    /// when its bytes cannot be read in scope.
    fn read(&mut self, reference: &str) -> Result<Option<(Value, usize)>> {
        Deadline::check(self.deadline, 0)?;
        if let Some((value, size)) = self.cache.get(reference) {
            // An erasure may have begun since the read that filled the
            // cache; what it withholds is withheld from here on.
            if self.store.payload_withheld(self.tenant, reference) {
                return Ok(None);
            }
            return Ok(Some((value.clone(), *size)));
        }
        let bytes = match self.readable.get(reference) {
            Some(false) => return Ok(None),
            // Reachability is already proven for this request; only the
            // erasure check is repeated before the file is read again.
            Some(true) => match self.store.payload_withheld(self.tenant, reference) {
                true => None,
                false => crate::payload::load_payload(&self.store.directory, reference)?,
            },
            None => {
                let bytes = self
                    .store
                    .payload_in_under(self.tenant, reference, self.deadline)?;
                self.readable.insert(reference.to_owned(), bytes.is_some());
                bytes
            }
        };
        let Some(bytes) = bytes else {
            return Ok(None);
        };
        // Offload only ever moves string attribute values, so a payload is
        // the UTF-8 text of one; anything else is not ours to reinterpret.
        let Ok(text) = String::from_utf8(bytes) else {
            self.readable.insert(reference.to_owned(), false);
            return Ok(None);
        };
        let size = text.len();
        let value = semconv::field_value(&Value::String(text));
        if self.cached_bytes.saturating_add(size) <= RESOLVE_CACHE_BYTES {
            self.cached_bytes += size;
            self.cache
                .insert(reference.to_owned(), (value.clone(), size));
        }
        Ok(Some((value, size)))
    }
}

/// Adds every `$payload` reference inside a STORED value to `into`.
pub(crate) fn collect_references(value: &Value, into: &mut std::collections::HashSet<String>) {
    if let Some(reference) = payload_reference(value) {
        into.insert(reference.to_owned());
        return;
    }
    match value {
        Value::Array(items) => items.iter().for_each(|item| collect_references(item, into)),
        Value::Object(fields) => fields
            .values()
            .for_each(|field| collect_references(field, into)),
        _ => {}
    }
}

/// The `sha256/<hex>` a `$payload` reference object names, or `None` for any
/// other value.
pub(crate) fn payload_reference(value: &Value) -> Option<&str> {
    value
        .as_object()?
        .get(crate::payload::PAYLOAD_KEY)?
        .as_str()
}

/// Whether `value` holds a `$payload` reference anywhere inside it.
#[cfg(test)]
pub(crate) fn contains_reference(value: &Value) -> bool {
    if payload_reference(value).is_some() {
        return true;
    }
    match value {
        Value::Array(items) => items.iter().any(contains_reference),
        Value::Object(fields) => fields.values().any(contains_reference),
        _ => false,
    }
}

/// One span as a tape step.
fn tape_step(
    index: usize,
    span: &Span,
    facts: StepFacts,
    extras: semconv::StepExtras,
    llm: LlmFacts,
) -> TapeStep {
    let attributes = &span.attributes;
    let config = if facts.kind == StepKind::Llm {
        attributes
            .iter()
            .filter(|(key, _)| key.starts_with(CONFIG_PREFIX) || *key == OI_INVOCATION_PARAMETERS)
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    } else {
        Map::new()
    };
    let detail = StepDetail {
        signature: facts.signature().to_owned(),
        tool_call_id: facts.tool_call_id.clone(),
        response_id: semconv::attr_str(attributes, "gen_ai.response.id"),
        finish_reasons: finish_reasons(attributes.get("gen_ai.response.finish_reasons")),
        config,
        tenant: span.tenant.clone(),
        session: llm.session.clone(),
        request_carriers: extras.request_carriers,
        response_source: extras.response_source,
        empty_response: extras.empty_response,
    };
    TapeStep {
        index,
        trace_id: span.trace_id.clone(),
        span_id: span.span_id.clone(),
        parent_span_id: span.parent_span_id.clone(),
        kind: facts.kind,
        name: span.name.clone(),
        service: span.service.clone(),
        model: llm.model,
        tool: facts.tool,
        agent: facts.agent,
        start_ns: span.start_time_ns,
        duration_ns: span.end_time_ns.saturating_sub(span.start_time_ns),
        status: span.status.clone(),
        request: facts.request,
        response: facts.response,
        request_digest: facts.request_digest,
        response_digest: facts.response_digest,
        fingerprint: facts.fingerprint,
        tokens: StepTokens {
            input: llm.prompt_tokens,
            output: llm.completion_tokens,
            cache_read: llm.cache_read_tokens,
            cache_write: llm.cache_creation_tokens,
        },
        cost_usd: llm.cost_usd,
        detail,
    }
}

/// `gen_ai.response.finish_reasons` read whichever way a producer wrote it: a
/// list of strings (the convention), one string, or a list JSON-encoded into a
/// string (exporters that cannot carry arrays).
fn finish_reasons(value: Option<&Value>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    match semconv::field_value(value) {
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item.as_str().filter(|text| !text.is_empty()))
            .map(str::to_owned)
            .collect(),
        Value::String(text) if !text.is_empty() => vec![text],
        _ => Vec::new(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// A store in a private temporary directory, removed on drop.
    pub(crate) struct TempStore {
        dir: std::path::PathBuf,
        store: Option<Store>,
    }

    impl TempStore {
        pub(crate) fn open(label: &str, config: crate::Config) -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let dir = std::env::temp_dir()
                .join(format!("traza-tape-{label}-{}-{nonce}", std::process::id()));
            std::fs::create_dir_all(&dir).expect("dir");
            let store = Store::open(&dir, config).expect("opens");
            Self {
                dir,
                store: Some(store),
            }
        }

        pub(crate) fn store(&self) -> &Store {
            self.store.as_ref().expect("open")
        }
    }

    impl Drop for TempStore {
        fn drop(&mut self) {
            drop(self.store.take());
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    pub(crate) fn span(trace: &str, id: &str, start: u64, attributes: Value) -> Span {
        Span {
            trace_id: trace.into(),
            span_id: id.into(),
            tenant: String::new(),
            parent_span_id: None,
            name: format!("op-{id}"),
            start_time_ns: start,
            end_time_ns: start + 10,
            status: "ok".into(),
            service: "agent".into(),
            attributes: attributes.as_object().expect("object").clone(),
            events: Vec::new(),
            links: Vec::new(),
            extra: Map::new(),
        }
    }

    fn seeded(label: &str, config: crate::Config) -> TempStore {
        let fixture = TempStore::open(label, config);
        fixture
            .store()
            .ingest_batch(vec![
                span(
                    "t1",
                    "root",
                    100,
                    json!({"session.id": "s1", "traceloop.span.kind": "workflow"}),
                ),
                span(
                    "t1",
                    "llm-1",
                    110,
                    json!({
                        "session.id": "s1",
                        "gen_ai.operation.name": "chat",
                        "gen_ai.request.model": "gpt-4o",
                        "gen_ai.request.temperature": 0.2,
                        "gen_ai.response.id": "chatcmpl-1",
                        "gen_ai.response.finish_reasons": ["tool_calls"],
                        "gen_ai.usage.input_tokens": 50,
                        "gen_ai.usage.output_tokens": 7,
                        "gen_ai.prompt.0.role": "user",
                        "gen_ai.prompt.0.content": "weather in Paris?",
                    }),
                ),
                span(
                    "t1",
                    "tool-1",
                    120,
                    json!({
                        "session.id": "s1",
                        "gen_ai.operation.name": "execute_tool",
                        "gen_ai.tool.name": "get_weather",
                        "gen_ai.tool.call.id": "call_1",
                        "gen_ai.tool.call.arguments": "{\"city\":\"Paris\"}",
                        "gen_ai.tool.call.result": "rainy",
                    }),
                ),
                span(
                    "t1",
                    "http",
                    125,
                    json!({"session.id": "s1", "http.method": "GET"}),
                ),
            ])
            .expect("ingests");
        fixture
    }

    #[test]
    fn a_tape_is_the_runs_steps_in_order_with_their_facts() {
        let fixture = seeded("order", crate::Config::default());
        let tape = fixture
            .store()
            .tape(Some(""), "s1", TapeOptions::default())
            .expect("reads")
            .expect("the run exists");
        // The workflow wrapper and the HTTP span are context, not steps.
        let kinds: Vec<&str> = tape.steps.iter().map(|step| step.kind.as_str()).collect();
        assert_eq!(kinds, ["llm", "tool"]);
        assert_eq!(tape.spans_examined, 4);
        assert!(!tape.truncated);
        assert_eq!(
            tape.resolved_partial, None,
            "absent unless resolution was asked"
        );

        let llm = &tape.steps[0];
        assert_eq!(llm.index, 0);
        assert_eq!(llm.model.as_deref(), Some("gpt-4o"));
        assert_eq!(llm.tokens.input, Some(50));
        assert_eq!(llm.tokens.cache_read, None, "unreported is not zero");
        assert_eq!(
            llm.request,
            Some(json!([{"role": "user", "content": "weather in Paris?"}]))
        );
        assert!(llm.fingerprint.starts_with("llm:gpt-4o#"));
        assert_eq!(llm.detail.signature, "llm:gpt-4o");
        assert_eq!(llm.detail.response_id.as_deref(), Some("chatcmpl-1"));
        assert_eq!(llm.detail.finish_reasons, ["tool_calls"]);
        assert_eq!(
            llm.detail.config.get("gen_ai.request.temperature"),
            Some(&json!(0.2))
        );

        let tool = &tape.steps[1];
        assert_eq!(tool.index, 1);
        assert_eq!(tool.tool.as_deref(), Some("get_weather"));
        assert_eq!(tool.request, Some(json!({"city": "Paris"})));
        assert_eq!(tool.response, Some(json!("rainy")));
        assert_eq!(tool.detail.tool_call_id.as_deref(), Some("call_1"));
        assert!(
            tool.detail.config.is_empty(),
            "only model calls carry config"
        );

        // The wire shape: optional facts are omitted, tokens are always an
        // object, and the replay-only detail never serializes.
        let wire = serde_json::to_value(tool).expect("serializes");
        assert!(wire.get("model").is_none() && wire.get("detail").is_none());
        assert_eq!(
            wire["tokens"],
            json!({"input": null, "output": null, "cache_read": null, "cache_write": null})
        );
        assert_eq!(wire["kind"], json!("tool"));
    }

    #[test]
    fn an_unknown_run_has_no_tape_and_the_step_cap_truncates() {
        let fixture = seeded("cap", crate::Config::default());
        let store = fixture.store();
        assert!(store
            .tape(Some(""), "nope", TapeOptions::default())
            .expect("reads")
            .is_none());
        let capped = store
            .tape(
                Some(""),
                "s1",
                TapeOptions {
                    max_steps: 1,
                    resolve: None,
                },
            )
            .expect("reads")
            .expect("exists");
        assert_eq!(capped.steps.len(), 1);
        assert!(capped.truncated);
        let exact = store
            .tape(
                Some(""),
                "s1",
                TapeOptions {
                    max_steps: 2,
                    resolve: None,
                },
            )
            .expect("reads")
            .expect("exists");
        assert!(
            !exact.truncated,
            "a tape that fits exactly is not truncated"
        );
    }

    #[test]
    fn resolution_inlines_payloads_as_a_prefix_within_the_budget() {
        // A 64-byte threshold offloads both long prompts below.
        let config = crate::Config {
            payload_threshold: Some(64),
            ..crate::Config::default()
        };
        let fixture = TempStore::open("resolve", config);
        let store = fixture.store();
        let first = format!(
            "[{{\"role\":\"user\",\"content\":\"{}\"}}]",
            "a".repeat(100)
        );
        let second = "b".repeat(200);
        store
            .ingest_batch(vec![
                span(
                    "t9",
                    "one",
                    1,
                    json!({"gen_ai.operation.name": "chat", "gen_ai.input.messages": first}),
                ),
                span(
                    "t9",
                    "two",
                    2,
                    json!({"gen_ai.operation.name": "chat", "gen_ai.prompt.0.content": second}),
                ),
            ])
            .expect("ingests");

        let stored = store
            .tape(Some(""), "trace:t9", TapeOptions::default())
            .expect("reads")
            .expect("exists");
        assert!(payload_reference(stored.steps[0].request.as_ref().expect("request")).is_some());

        let resolved = store
            .tape(
                Some(""),
                "trace:t9",
                TapeOptions {
                    max_steps: 10,
                    resolve: Some(1024),
                },
            )
            .expect("reads")
            .expect("exists");
        assert_eq!(resolved.resolved_partial, Some(false));
        // The resolved JSON text is read as JSON, exactly as inline text is.
        assert_eq!(
            resolved.steps[0].request,
            Some(json!([{"role": "user", "content": "a".repeat(100)}]))
        );
        assert_eq!(
            resolved.steps[1].request,
            Some(json!([{"content": second.clone()}]))
        );
        // Identities describe the stored form whether or not it was resolved.
        for (left, right) in stored.steps.iter().zip(&resolved.steps) {
            assert_eq!(left.request_digest, right.request_digest);
            assert_eq!(left.fingerprint, right.fingerprint);
        }

        // A budget that fits the first payload but not the second resolves
        // the first and leaves the rest as references, and says so.
        let partial = store
            .tape(
                Some(""),
                "trace:t9",
                TapeOptions {
                    max_steps: 10,
                    resolve: Some(150),
                },
            )
            .expect("reads")
            .expect("exists");
        assert_eq!(partial.resolved_partial, Some(true));
        assert!(!contains_reference(
            partial.steps[0].request.as_ref().expect("request")
        ));
        assert!(contains_reference(
            partial.steps[1].request.as_ref().expect("request")
        ));
    }

    #[test]
    fn a_resolved_message_list_takes_its_system_instructions_as_inline() {
        // Offloaded messages beside inline system instructions, and the
        // reverse: once resolved, each request is exactly what it would have
        // been had nothing been offloaded.
        let config = crate::Config {
            payload_threshold: Some(64),
            ..crate::Config::default()
        };
        let fixture = TempStore::open("resolve-system", config);
        let store = fixture.store();
        let messages =
            json!([{"role": "user", "parts": [{"type": "text", "content": "m".repeat(100)}]}]);
        let parts = json!([{"type": "text", "content": "s".repeat(100)}]);
        let short_messages =
            json!([{"role": "user", "parts": [{"type": "text", "content": "hi"}]}]);
        store
            .ingest_batch(vec![
                span(
                    "t-sys",
                    "long-messages",
                    1,
                    json!({"gen_ai.operation.name": "chat",
                           "gen_ai.system_instructions": "Be brief.",
                           "gen_ai.input.messages": messages.to_string()}),
                ),
                span(
                    "t-sys",
                    "long-system",
                    2,
                    json!({"gen_ai.operation.name": "chat",
                           "gen_ai.system_instructions": parts.to_string(),
                           "gen_ai.input.messages": short_messages.to_string()}),
                ),
            ])
            .expect("ingests");
        let stored = store
            .tape(Some(""), "trace:t-sys", TapeOptions::default())
            .expect("reads")
            .expect("exists");
        // Stored, both carriers are there, the offloaded one as a reference.
        let first = stored.steps[0].request.as_ref().expect("request");
        assert_eq!(first[0], json!({"role": "system", "content": "Be brief."}));
        assert!(payload_reference(&first[1]).is_some(), "{first}");
        let resolved = store
            .tape(
                Some(""),
                "trace:t-sys",
                TapeOptions {
                    max_steps: 10,
                    resolve: Some(1 << 20),
                },
            )
            .expect("reads")
            .expect("exists");
        assert_eq!(resolved.resolved_partial, Some(false));
        let mut inline_first = messages.as_array().unwrap().clone();
        inline_first.insert(0, json!({"role": "system", "content": "Be brief."}));
        assert_eq!(resolved.steps[0].request, Some(json!(inline_first)));
        // The offloaded parts list is a parts message, as inline parts are.
        let mut inline_second = short_messages.as_array().unwrap().clone();
        inline_second.insert(0, json!({"role": "system", "parts": parts}));
        assert_eq!(resolved.steps[1].request, Some(json!(inline_second)));
    }

    #[test]
    fn resolution_spends_the_callers_compute_budget() {
        let config = crate::Config {
            payload_threshold: Some(64),
            ..crate::Config::default()
        };
        let fixture = TempStore::open("resolve-deadline", config);
        let store = fixture.store();
        store
            .ingest_batch(vec![span(
                "t8",
                "one",
                1,
                json!({"gen_ai.operation.name": "chat", "gen_ai.prompt.0.content": "q".repeat(100)}),
            )])
            .expect("ingests");
        let stored = store
            .tape(Some(""), "trace:t8", TapeOptions::default())
            .expect("reads")
            .expect("exists");
        let request = stored.steps[0].request.clone().expect("request");
        assert!(contains_reference(&request));

        // A budget already spent: resolving refuses at the first reference,
        // under a tenant scope (whose reachability probe used to start a
        // budget of its own) and in the operator view alike.
        let spent = crate::Deadline::starting(Instant::now(), Some(std::time::Duration::ZERO));
        for tenant in [Some(""), None] {
            let mut value = request.clone();
            let mut resolver = Resolver::new(store, tenant, spent);
            assert!(
                matches!(
                    resolver.resolve(&mut value, &mut ResolveBudget::new(1 << 20)),
                    Err(crate::Error::DeadlineExceeded(_))
                ),
                "{tenant:?}"
            );
        }
        // With budget left, the same reference resolves — and a repeat is
        // served from what the first read learned.
        let mut resolver = Resolver::new(store, Some(""), None);
        let mut budget = ResolveBudget::new(1 << 20);
        for _ in 0..2 {
            let mut value = request.clone();
            assert_eq!(
                resolver.resolve(&mut value, &mut budget).expect("resolves"),
                Leftover::default()
            );
            assert_eq!(value, json!([{"content": "q".repeat(100)}]));
        }
        assert_eq!(budget.spent(), 200, "each occurrence is charged");
        assert_eq!(resolver.readable.len(), 1, "probed once");

        // A reference whose bytes are gone stays a reference, as unreadable.
        let mut gone = json!({"$payload": format!("sha256/{}", "0".repeat(64))});
        assert_eq!(
            resolver
                .resolve(&mut gone, &mut budget)
                .expect("not an error"),
            Leftover {
                past_budget: false,
                unreadable: true
            }
        );
    }

    #[test]
    fn finish_reasons_read_every_spelling() {
        assert_eq!(finish_reasons(Some(&json!(["stop"]))), ["stop"]);
        assert_eq!(finish_reasons(Some(&json!("length"))), ["length"]);
        assert_eq!(
            finish_reasons(Some(&json!("[\"tool_calls\"]"))),
            ["tool_calls"]
        );
        assert!(finish_reasons(Some(&json!(""))).is_empty());
        assert!(finish_reasons(None).is_empty());
    }
}
