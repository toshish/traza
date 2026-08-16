//! Realistic synthetic telemetry: the corpus the scenario tests assert
//! against and the `seed` binary loads for manual and UI work.
//!
//! The point is COVERAGE OF REAL SHAPES, not volume for its own sake. One
//! corpus contains agentic tool-calling traces, multi-vendor LLM calls under
//! all three attribute dialects Traza accepts (current OTel GenAI, the
//! deprecated GenAI names, and Traza's native `llm.*` shorthand), long
//! multi-turn sessions, multimodal messages (image/audio/video/document),
//! RAG with embeddings and a vector store, streaming, failures and linked
//! retries, parallel fan-out and join, oversized payloads that must offload,
//! and ordinary non-LLM service spans that must never be counted as LLM work.
//!
//! Generation is DETERMINISTIC: the same [`SeedOptions`] always produce the
//! same corpus, byte for byte, so a failing scenario test is reproducible and
//! a seeded store can be regenerated exactly.

use serde_json::{json, Map, Value};

use crate::annotations::Annotation;
use crate::{Event, Link, Span};

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;

/// Deterministic PRNG (SplitMix64). A dependency-free generator keeps the
/// corpus reproducible without pulling in `rand`.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_add(0x9e37_79b9_7f4a_7c15))
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `lo..=hi`.
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        if hi <= lo {
            return lo;
        }
        lo + self.next_u64() % (hi - lo + 1)
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[(self.next_u64() % items.len() as u64) as usize]
    }

    /// True `percent` of the time.
    fn chance(&mut self, percent: u64) -> bool {
        self.next_u64() % 100 < percent
    }
}

/// Which attribute dialect a session's spans speak. Traza accepts all three;
/// the corpus mixes them so rollups are proven convention-independent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    /// Current OTel GenAI names (`gen_ai.provider.name`, `input_tokens`, …).
    Current,
    /// OTel-deprecated names still emitted by older instrumentation
    /// (`gen_ai.system`, `prompt_tokens`/`completion_tokens`).
    Deprecated,
    /// Traza's native shorthand (`llm.model`, `llm.prompt_tokens`, …).
    Native,
}

/// A provider/model pair with per-1K-token costs, used to meter `llm.cost_usd`.
struct Vendor {
    provider: &'static str,
    model: &'static str,
    service: &'static str,
    prompt_per_1k: f64,
    completion_per_1k: f64,
}

const VENDORS: &[Vendor] = &[
    Vendor {
        provider: "openai",
        model: "gpt-4o",
        service: "support-agent",
        prompt_per_1k: 0.0025,
        completion_per_1k: 0.010,
    },
    Vendor {
        provider: "openai",
        model: "gpt-4o-mini",
        service: "support-agent",
        prompt_per_1k: 0.00015,
        completion_per_1k: 0.0006,
    },
    Vendor {
        provider: "anthropic",
        model: "claude-sonnet-4-5",
        service: "research-agent",
        prompt_per_1k: 0.003,
        completion_per_1k: 0.015,
    },
    Vendor {
        provider: "aws.bedrock",
        model: "anthropic.claude-3-5-sonnet-20241022-v2:0",
        service: "batch-worker",
        prompt_per_1k: 0.003,
        completion_per_1k: 0.015,
    },
    Vendor {
        provider: "gcp.vertex_ai",
        model: "gemini-2.0-flash",
        service: "research-agent",
        prompt_per_1k: 0.00010,
        completion_per_1k: 0.0004,
    },
    Vendor {
        provider: "azure.ai.openai",
        model: "gpt-4o",
        service: "checkout-copilot",
        prompt_per_1k: 0.0025,
        completion_per_1k: 0.010,
    },
    Vendor {
        provider: "ollama",
        model: "llama3.1:8b",
        service: "edge-agent",
        prompt_per_1k: 0.0,
        completion_per_1k: 0.0,
    },
    Vendor {
        provider: "cohere",
        model: "command-r-plus",
        service: "batch-worker",
        prompt_per_1k: 0.0025,
        completion_per_1k: 0.010,
    },
];

const TOOLS: &[(&str, &str)] = &[
    ("web_search", "{\"query\":\"traza tracing datastore\"}"),
    (
        "get_weather",
        "{\"location\":\"Paris\",\"unit\":\"celsius\"}",
    ),
    ("sql_query", "{\"sql\":\"select count(*) from orders\"}"),
    (
        "send_email",
        "{\"to\":\"ops@example.com\",\"subject\":\"digest\"}",
    ),
    ("read_file", "{\"path\":\"/srv/reports/q4.pdf\"}"),
    (
        "create_ticket",
        "{\"title\":\"Refund request\",\"priority\":\"high\"}",
    ),
];

/// Demo media is synthesized (see [`crate::media`]) rather than pasted as a
/// token blob: the renderer is only exercised by bytes that actually decode
/// into something a reader can see and hear.
fn demo_image_png() -> String {
    crate::media::data_uri("image/png", &crate::media::png_chart(480, 260))
}

fn demo_image_svg() -> String {
    crate::media::data_uri("image/svg+xml", crate::media::svg_chart().as_bytes())
}

/// Kept deliberately small: the encoder emits uncompressed LZW, so pixels
/// cost ~9 bits each, and an animation past the payload threshold would be
/// offloaded to the payload store and never render inline — which defeats
/// the point of seeding a moving picture at all.
fn demo_animation_gif() -> String {
    crate::media::data_uri("image/gif", &crate::media::gif_animation(160, 90, 6))
}

fn demo_audio_wav() -> String {
    crate::media::data_uri("audio/wav", &crate::media::wav_arpeggio(3.0))
}

/// A markdown answer, the most common assistant output shape.
const MARKDOWN_ANSWER: &str = "## Q4 summary\n\nRevenue rose **12%** quarter over quarter, driven by:\n\n1. Enterprise renewals (up 18%)\n2. A pricing change in *EMEA*\n3. Lower churn — see `retention.sql`\n\n> Margin was flat; support costs absorbed the gain.\n\n| Region | Revenue | Change |\n|---|---|---|\n| NA | $4.2M | +9% |\n| EMEA | $2.8M | +21% |\n| APAC | $1.1M | +4% |\n\nNext step: confirm the EMEA numbers with finance.";

/// A fenced code answer.
const CODE_ANSWER: &str = "Here is the query you asked for:\n\n```sql\nselect\n  date_trunc('month', created_at) as month,\n  count(*) as orders,\n  sum(total_cents) / 100.0 as revenue\nfrom orders\nwhere created_at >= now() - interval '12 months'\ngroup by 1\norder by 1;\n```\n\nRun it against the replica — it scans the whole orders table.";

/// A structured-output (JSON) answer.
const JSON_ANSWER: &str = "{\n  \"sentiment\": \"negative\",\n  \"confidence\": 0.87,\n  \"topics\": [\"billing\", \"refund\", \"support wait time\"],\n  \"entities\": [\n    {\"type\": \"order_id\", \"value\": \"A-441902\"},\n    {\"type\": \"amount\", \"value\": 129.99, \"currency\": \"USD\"}\n  ],\n  \"escalate\": true\n}";

/// How the corpus is generated.
#[derive(Clone, Debug)]
pub struct SeedOptions {
    /// Multiplier on the number of sessions per scenario. `1` is the compact
    /// corpus the scenario tests use; larger values are for load work.
    pub scale: usize,
    /// Start of the generated time window (Unix nanoseconds). Spans are laid
    /// out forward from here across several days.
    pub start_time_ns: u64,
    /// PRNG seed; the same seed reproduces the corpus exactly.
    pub seed: u64,
    /// Distinguishes the ids of one batch from another. Span ids are numbered
    /// from zero within a corpus, so generating a large store in chunks needs
    /// a per-chunk namespace or the chunks would collide on the primary key
    /// and silently upsert each other. Empty for a single corpus.
    pub namespace: String,
    /// Size of the deliberately oversized prompt bodies, which must exceed the
    /// server's payload threshold to exercise offloading.
    pub big_payload_bytes: usize,
}

impl Default for SeedOptions {
    fn default() -> Self {
        Self {
            scale: 1,
            // 2026-01-05T00:00:00Z — a fixed, recognizable window.
            start_time_ns: 1_767_571_200 * SEC,
            seed: 0x7_4a_2a,
            namespace: String::new(),
            big_payload_bytes: 300 * 1024,
        }
    }
}

/// A generated corpus: spans plus the post-hoc judgments attached to them.
#[derive(Clone, Debug, Default)]
pub struct Corpus {
    /// Every generated span, in generation order (not sorted by time).
    pub spans: Vec<Span>,
    /// Eval scores and human feedback referencing spans in `spans`.
    pub annotations: Vec<Annotation>,
}

impl Corpus {
    /// Total spans generated.
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    /// True when nothing was generated.
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }
}

/// Builds the corpus described by `options`.
pub fn corpus(options: &SeedOptions) -> Corpus {
    let mut gen = Gen {
        rng: Rng::new(options.seed),
        clock: options.start_time_ns,
        options: options.clone(),
        out: Corpus::default(),
        counter: 0,
    };
    let scale = options.scale.max(1);

    for index in 0..(3 * scale) {
        gen.tool_calling_agent(index);
    }
    for index in 0..(2 * scale) {
        gen.multi_turn_session(index);
    }
    for index in 0..(2 * scale) {
        gen.rag_pipeline(index);
    }
    for index in 0..(2 * scale) {
        gen.multimodal_session(index);
    }
    for index in 0..(2 * scale) {
        gen.failure_and_retry(index);
    }
    for index in 0..scale {
        gen.parallel_fanout(index);
    }
    for index in 0..scale {
        gen.runaway_research_agent(index);
    }
    for index in 0..scale {
        gen.bulk_enrichment_fanout(index);
    }
    for index in 0..scale {
        gen.oversized_payloads(index);
    }
    for index in 0..(2 * scale) {
        gen.streaming_chat(index);
    }
    for index in 0..(3 * scale) {
        gen.plain_service_traffic(index);
    }
    // Framework- and provider-shaped traces: the same conventions arriving in
    // the different arrangements real SDKs produce.
    for index in 0..(2 * scale) {
        gen.openai_session(index);
    }
    for index in 0..(2 * scale) {
        gen.anthropic_session(index);
    }
    for index in 0..scale {
        gen.langgraph_session(index);
    }
    for index in 0..scale {
        gen.crewai_session(index);
    }
    for index in 0..scale {
        gen.generated_media_session(index);
    }
    for index in 0..scale {
        gen.content_formats_session(index);
    }
    gen.out
}

struct Gen {
    rng: Rng,
    clock: u64,
    options: SeedOptions,
    out: Corpus,
    counter: u64,
}

impl Gen {
    fn id(&mut self, prefix: &str) -> String {
        self.counter += 1;
        if self.options.namespace.is_empty() {
            format!("{prefix}-{:06x}", self.counter)
        } else {
            format!("{prefix}-{}-{:06x}", self.options.namespace, self.counter)
        }
    }

    /// Advances the shared clock so traces spread realistically over days.
    fn advance(&mut self, by_ns: u64) -> u64 {
        self.clock += by_ns;
        self.clock
    }

    /// Advances by a random multiple of `unit`, in `lo..=hi`.
    fn advance_rand(&mut self, lo: u64, hi: u64, unit: u64) -> u64 {
        let step = self.rng.range(lo, hi) * unit;
        self.advance(step)
    }

    fn push(&mut self, span: Span) {
        self.out.spans.push(span);
    }

    /// Usage/model/provider attributes in the given dialect, plus a metered
    /// `llm.cost_usd` (a Traza extension — no GenAI convention carries cost).
    fn usage_attributes(
        &mut self,
        vendor: &Vendor,
        dialect: Dialect,
        prompt_tokens: u64,
        completion_tokens: u64,
    ) -> Map<String, Value> {
        let mut attributes = Map::new();
        let cost = (prompt_tokens as f64 / 1000.0) * vendor.prompt_per_1k
            + (completion_tokens as f64 / 1000.0) * vendor.completion_per_1k;
        match dialect {
            Dialect::Current => {
                attributes.insert("gen_ai.provider.name".into(), json!(vendor.provider));
                attributes.insert("gen_ai.operation.name".into(), json!("chat"));
                attributes.insert("gen_ai.request.model".into(), json!(vendor.model));
                attributes.insert("gen_ai.response.model".into(), json!(vendor.model));
                attributes.insert("gen_ai.usage.input_tokens".into(), json!(prompt_tokens));
                attributes.insert(
                    "gen_ai.usage.output_tokens".into(),
                    json!(completion_tokens),
                );
            }
            Dialect::Deprecated => {
                attributes.insert("gen_ai.system".into(), json!(vendor.provider));
                attributes.insert("gen_ai.request.model".into(), json!(vendor.model));
                attributes.insert("gen_ai.usage.prompt_tokens".into(), json!(prompt_tokens));
                attributes.insert(
                    "gen_ai.usage.completion_tokens".into(),
                    json!(completion_tokens),
                );
                attributes.insert(
                    "llm.usage.total_tokens".into(),
                    json!(prompt_tokens + completion_tokens),
                );
            }
            Dialect::Native => {
                attributes.insert("llm.model".into(), json!(vendor.model));
                attributes.insert("llm.prompt_tokens".into(), json!(prompt_tokens));
                attributes.insert("llm.completion_tokens".into(), json!(completion_tokens));
            }
        }
        if cost > 0.0 {
            attributes.insert("llm.cost_usd".into(), json!(round_cents(cost)));
        }
        attributes.insert(
            "gen_ai.request.temperature".into(),
            json!(*self.rng.pick(&[0.0_f64, 0.2, 0.7, 1.0])),
        );
        attributes.insert("gen_ai.request.max_tokens".into(), json!(2048));
        attributes
    }

    // ---------------------------------------------------------- scenarios

    /// An agent workflow: plan → LLM decides on a tool → tool executes → LLM
    /// answers. Exercises `traceloop.span.kind`, nesting, and tool-call
    /// message parts.
    fn tool_calling_agent(&mut self, index: usize) {
        let vendor = *self.rng.pick(&[0_usize, 1, 2, 5]);
        let vendor = &VENDORS[vendor];
        let dialect = Dialect::Current;
        let session = format!("chat-{:04}", 100 + index);
        let trace = self.id("trace-agent");
        let root = self.id("span");
        let start = self.advance_rand(30, 400, SEC);
        let (tool_name, tool_args) = *self.rng.pick(TOOLS);

        // Workflow root.
        let mut attributes = Map::new();
        attributes.insert("traceloop.span.kind".into(), json!("workflow"));
        attributes.insert("traceloop.workflow.name".into(), json!("answer_question"));
        attributes.insert("traceloop.entity.name".into(), json!("answer_question"));
        attributes.insert("gen_ai.conversation.id".into(), json!(session));
        attributes.insert(
            "traceloop.association.properties.user_id".into(),
            json!(format!("user-{:03}", index % 40)),
        );
        let total = self.rng.range(900, 4200) * MS;
        let mut workflow = make_span(
            &trace,
            &root,
            None,
            "answer_question.workflow",
            vendor.service,
            start,
            total,
            "ok",
            attributes,
        );
        workflow.events.push(Event {
            name: "workflow.start".into(),
            timestamp_ns: start,
            attributes: Map::new(),
        });
        self.push(workflow);

        // 1) The model asks for a tool.
        let decide = self.id("span");
        let prompt_tokens = self.rng.range(300, 1500);
        let completion_tokens = self.rng.range(20, 120);
        let mut attributes =
            self.usage_attributes(vendor, dialect, prompt_tokens, completion_tokens);
        attributes.insert("traceloop.span.kind".into(), json!("llm"));
        attributes.insert("gen_ai.conversation.id".into(), json!(session));
        attributes.insert("gen_ai.response.finish_reason".into(), json!("tool_calls"));
        attributes.insert(
            "gen_ai.input.messages".into(),
            messages_json(json!([
                {"role": "system", "parts": [{"type": "text", "content": "You are a support agent. Use tools when needed."}]},
                {"role": "user", "parts": [{"type": "text", "content": "Can you look this up for me?"}]}
            ])),
        );
        attributes.insert(
            "gen_ai.output.messages".into(),
            messages_json(json!([
                {"role": "assistant", "parts": [{
                    "type": "tool_call",
                    "id": format!("call_{index:04}"),
                    "name": tool_name,
                    "arguments": serde_json::from_str::<Value>(tool_args).unwrap_or(json!({}))
                }], "finish_reason": "tool_calls"}
            ])),
        );
        let decide_ms = self.rng.range(200, 900) * MS;
        let span = make_span(
            &trace,
            &decide,
            Some(&root),
            &format!("{}.chat", vendor.provider),
            vendor.service,
            start + 5 * MS,
            decide_ms,
            "ok",
            attributes,
        );
        self.push(span);

        // 2) The tool runs.
        let tool_span = self.id("span");
        let mut attributes = Map::new();
        attributes.insert("traceloop.span.kind".into(), json!("tool"));
        attributes.insert("traceloop.entity.name".into(), json!(tool_name));
        attributes.insert("gen_ai.tool.name".into(), json!(tool_name));
        attributes.insert(
            "gen_ai.tool.call.id".into(),
            json!(format!("call_{index:04}")),
        );
        attributes.insert("gen_ai.conversation.id".into(), json!(session));
        attributes.insert("traceloop.entity.input".into(), json!(tool_args));
        let tool_failed = self.rng.chance(12);
        attributes.insert(
            "traceloop.entity.output".into(),
            json!(if tool_failed {
                "{\"error\":\"upstream timeout after 5000ms\"}"
            } else {
                "{\"result\":\"rainy, 14C\"}"
            }),
        );
        if tool_failed {
            attributes.insert("error.type".into(), json!("UpstreamTimeout"));
        }
        let tool_ms = self.rng.range(40, 2500) * MS;
        let span = make_span(
            &trace,
            &tool_span,
            Some(&decide),
            &format!("{tool_name}.tool"),
            vendor.service,
            start + 5 * MS + decide_ms,
            tool_ms,
            if tool_failed { "error" } else { "ok" },
            attributes,
        );
        self.push(span);

        // 3) The model answers with the tool result in context.
        let answer = self.id("span");
        let prompt_tokens = self.rng.range(600, 2600);
        let completion_tokens = self.rng.range(80, 700);
        let mut attributes =
            self.usage_attributes(vendor, dialect, prompt_tokens, completion_tokens);
        attributes.insert("traceloop.span.kind".into(), json!("llm"));
        attributes.insert("gen_ai.conversation.id".into(), json!(session));
        attributes.insert("gen_ai.response.finish_reason".into(), json!("stop"));
        attributes.insert(
            "gen_ai.input.messages".into(),
            messages_json(json!([
                {"role": "user", "parts": [{"type": "text", "content": "Can you look this up for me?"}]},
                {"role": "tool", "parts": [{
                    "type": "tool_call_response",
                    "id": format!("call_{index:04}"),
                    "result": if tool_failed { "error: upstream timeout" } else { "rainy, 14C" }
                }]}
            ])),
        );
        attributes.insert(
            "gen_ai.output.messages".into(),
            messages_json(json!([
                {"role": "assistant", "parts": [{"type": "text", "content": "It is currently rainy and 14C."}], "finish_reason": "stop"}
            ])),
        );
        let span = make_span(
            &trace,
            &answer,
            Some(&root),
            &format!("{}.chat", vendor.provider),
            vendor.service,
            start + 10 * MS + decide_ms + tool_ms,
            self.rng.range(300, 1800) * MS,
            "ok",
            attributes,
        );
        self.push(span);

        // Post-hoc judgment on the answer.
        if self.rng.chance(45) {
            let score = (self.rng.range(60, 100) as f64) / 100.0;
            self.out.annotations.push(Annotation {
                trace_id: trace.clone(),
                span_id: answer.clone(),
                tenant: String::new(),
                session_id: String::new(),
                experiment_id: None,
                example_id: String::new(),
                name: "groundedness".into(),
                value: json!(score),
                source: "eval:nightly".into(),
                comment: String::new(),
                timestamp_ns: start + total + SEC,
            });
        }
    }

    /// A long conversation: many traces sharing one session id, so session
    /// rollups must aggregate across traces and segments.
    fn multi_turn_session(&mut self, index: usize) {
        let vendor = &VENDORS[(index + 2) % VENDORS.len()];
        let dialect = *self
            .rng
            .pick(&[Dialect::Current, Dialect::Deprecated, Dialect::Native]);
        let session = format!("thread-{:04}", 500 + index);
        let turns = self.rng.range(12, 40);
        let mut history = 400_u64;
        for turn in 0..turns {
            let trace = self.id("trace-turn");
            let span_id = self.id("span");
            let start = self.advance_rand(5, 90, SEC);
            let prompt_tokens = history;
            let completion_tokens = self.rng.range(40, 400);
            history += prompt_tokens / 8 + completion_tokens;
            let mut attributes =
                self.usage_attributes(vendor, dialect, prompt_tokens, completion_tokens);
            // Long sessions mix the session key across dialects, which is
            // exactly the mixed-convention case the session filter unions.
            match dialect {
                Dialect::Native => {
                    attributes.insert("session.id".into(), json!(session));
                }
                _ => {
                    attributes.insert("gen_ai.conversation.id".into(), json!(session));
                }
            }
            attributes.insert(
                "traceloop.association.properties.chat_id".into(),
                json!(session),
            );
            attributes.insert("gen_ai.response.finish_reason".into(), json!("stop"));
            attributes.insert(
                "gen_ai.input.messages".into(),
                messages_json(json!([
                    {"role": "user", "parts": [{"type": "text", "content": format!("Follow-up question #{turn}")}]}
                ])),
            );
            attributes.insert(
                "gen_ai.output.messages".into(),
                messages_json(json!([
                    {"role": "assistant", "parts": [{"type": "text", "content": format!("Answer to #{turn}.")}], "finish_reason": "stop"}
                ])),
            );
            let span = make_span(
                &trace,
                &span_id,
                None,
                &format!("{}.chat", vendor.provider),
                vendor.service,
                start,
                self.rng.range(400, 5_000) * MS,
                "ok",
                attributes,
            );
            self.push(span);
        }
    }

    /// Retrieval-augmented generation: embed the query, search a vector store,
    /// then answer. Covers the embedding operation and non-GenAI `db.*` spans.
    fn rag_pipeline(&mut self, index: usize) {
        let vendor = &VENDORS[index % VENDORS.len()];
        let session = format!("rag-{:04}", 900 + index);
        let trace = self.id("trace-rag");
        let root = self.id("span");
        let start = self.advance_rand(20, 300, SEC);

        let mut attributes = Map::new();
        attributes.insert("traceloop.span.kind".into(), json!("workflow"));
        attributes.insert("traceloop.workflow.name".into(), json!("rag_answer"));
        attributes.insert("gen_ai.conversation.id".into(), json!(session));
        let span = make_span(
            &trace,
            &root,
            None,
            "rag_answer.workflow",
            vendor.service,
            start,
            self.rng.range(500, 2600) * MS,
            "ok",
            attributes,
        );
        self.push(span);

        // Embedding call: input tokens only, no completion.
        let embed = self.id("span");
        let embed_tokens = self.rng.range(20, 400);
        let mut attributes = Map::new();
        attributes.insert("gen_ai.provider.name".into(), json!("openai"));
        attributes.insert("gen_ai.operation.name".into(), json!("embeddings"));
        attributes.insert(
            "gen_ai.request.model".into(),
            json!("text-embedding-3-small"),
        );
        attributes.insert("gen_ai.usage.input_tokens".into(), json!(embed_tokens));
        attributes.insert("llm.request.type".into(), json!("embedding"));
        attributes.insert("gen_ai.conversation.id".into(), json!(session));
        attributes.insert(
            "llm.cost_usd".into(),
            json!(round_cents(embed_tokens as f64 / 1000.0 * 0.00002)),
        );
        let span = make_span(
            &trace,
            &embed,
            Some(&root),
            "openai.embeddings",
            vendor.service,
            start + 3 * MS,
            self.rng.range(20, 200) * MS,
            "ok",
            attributes,
        );
        self.push(span);

        // Vector search: a plain db span, not an LLM call.
        let search = self.id("span");
        let store = *self.rng.pick(&["chroma", "pinecone", "qdrant", "pgvector"]);
        let mut attributes = Map::new();
        attributes.insert("db.system".into(), json!(store));
        attributes.insert("db.operation".into(), json!("query"));
        attributes.insert("db.vector.query.top_k".into(), json!(self.rng.range(3, 20)));
        attributes.insert("gen_ai.conversation.id".into(), json!(session));
        let span = make_span(
            &trace,
            &search,
            Some(&root),
            &format!("{store}.query"),
            vendor.service,
            start + 30 * MS,
            self.rng.range(5, 300) * MS,
            "ok",
            attributes,
        );
        self.push(span);

        // Answer grounded in the retrieved chunks.
        let answer = self.id("span");
        let prompt_tokens = self.rng.range(1500, 6000);
        let completion_tokens = self.rng.range(100, 800);
        let mut attributes =
            self.usage_attributes(vendor, Dialect::Current, prompt_tokens, completion_tokens);
        attributes.insert("traceloop.span.kind".into(), json!("llm"));
        attributes.insert("gen_ai.conversation.id".into(), json!(session));
        attributes.insert(
            "gen_ai.input.messages".into(),
            messages_json(json!([
                {"role": "system", "parts": [{"type": "text", "content": "Answer only from the provided context."}]},
                {"role": "user", "parts": [{"type": "text", "content": "What changed in the Q4 report?"}]}
            ])),
        );
        attributes.insert(
            "gen_ai.output.messages".into(),
            messages_json(json!([
                {"role": "assistant", "parts": [{"type": "text", "content": "Revenue rose 12% quarter over quarter."}], "finish_reason": "stop"}
            ])),
        );
        let span = make_span(
            &trace,
            &answer,
            Some(&root),
            &format!("{}.chat", vendor.provider),
            vendor.service,
            start + 200 * MS,
            self.rng.range(300, 2000) * MS,
            "ok",
            attributes,
        );
        self.push(span);
    }

    /// Multimodal turns: image, audio, video, and document parts alongside
    /// text, including one inline base64 blob big enough to matter to the UI.
    fn multimodal_session(&mut self, index: usize) {
        let vendor = &VENDORS[*self.rng.pick(&[0_usize, 2, 4])];
        let session = format!("media-{:04}", 700 + index);
        let modalities: &[(&str, &str, &str)] = &[
            ("image", "image/png", "screenshot-2026-01-05.png"),
            ("audio", "audio/mpeg", "support-call-4471.mp3"),
            ("video", "video/mp4", "onboarding-clip.mp4"),
            ("document", "application/pdf", "q4-report.pdf"),
        ];
        for (turn, (kind, mime, filename)) in modalities.iter().enumerate() {
            let trace = self.id("trace-media");
            let span_id = self.id("span");
            let start = self.advance_rand(10, 120, SEC);
            let prompt_tokens = self.rng.range(800, 5000);
            let completion_tokens = self.rng.range(50, 500);
            let mut attributes =
                self.usage_attributes(vendor, Dialect::Current, prompt_tokens, completion_tokens);
            attributes.insert("gen_ai.conversation.id".into(), json!(session));
            attributes.insert("gen_ai.output.type".into(), json!("text"));
            let size_bytes = self.rng.range(24_000, 6_000_000);
            // The first turn carries the payload INLINE as base64 (the shape
            // that bloats a naive UI); the rest reference it by URI.
            let part = if turn == 0 {
                json!({
                    "type": kind,
                    "mime_type": mime,
                    "filename": filename,
                    "size_bytes": size_bytes,
                    "data": format!("data:{mime};base64,{}", "iVBORw0KGgoAAAANSUhEUg".repeat(600))
                })
            } else {
                json!({
                    "type": kind,
                    "mime_type": mime,
                    "filename": filename,
                    "size_bytes": size_bytes,
                    "uri": format!("s3://traza-demo/media/{filename}")
                })
            };
            attributes.insert(
                "gen_ai.input.messages".into(),
                messages_json(json!([
                    {"role": "user", "parts": [
                        {"type": "text", "content": format!("What is in this {kind}?")},
                        part
                    ]}
                ])),
            );
            attributes.insert(
                "gen_ai.output.messages".into(),
                messages_json(json!([
                    {"role": "assistant", "parts": [{"type": "text", "content": format!("The {kind} shows a quarterly summary.")}], "finish_reason": "stop"}
                ])),
            );
            let span = make_span(
                &trace,
                &span_id,
                None,
                &format!("{}.chat", vendor.provider),
                vendor.service,
                start,
                self.rng.range(800, 9000) * MS,
                "ok",
                attributes,
            );
            self.push(span);
        }

        // A fifth turn covering the OTHER spellings of "here is media" the
        // dashboard must render: bare base64 with a MIME type (no data:
        // prefix), the Google inline_data shape, an MCP-style tool result
        // carrying a screenshot, and a part whose bytes the emitter declined
        // to capture — which must present its reason, not an empty frame.
        // Real bytes as elsewhere in this corpus: a chart small enough to
        // stay inline under the default offload threshold.
        let raw_base64 = crate::media::base64_encode(&crate::media::png_chart(160, 90));
        let trace = self.id("trace-media");
        let span_id = self.id("span");
        let start = self.advance_rand(10, 120, SEC);
        let prompt_tokens = self.rng.range(800, 5000);
        let mut attributes = self.usage_attributes(vendor, Dialect::Current, prompt_tokens, 120);
        attributes.insert("gen_ai.conversation.id".into(), json!(session));
        attributes.insert(
            "gen_ai.input.messages".into(),
            messages_json(json!([
                {"role": "user", "parts": [
                    {"type": "text", "content": "Compare the two charts; one attachment could not be captured."},
                    {"type": "image", "mime_type": "image/png", "filename": "chart-raw-b64.png",
                     "size_bytes": raw_base64.len() * 3 / 4, "data": raw_base64},
                    {"inline_data": {"mime_type": "image/png", "data": raw_base64}},
                    {"type": "image", "mime_type": "image/jpeg", "filename": "outside-roots.jpg",
                     "size_bytes": 38399, "archive_status": "unavailable",
                     "capture_status": "unavailable", "unavailable_reason": "outside_allowed_roots"}
                ]}
            ])),
        );
        attributes.insert(
            "gen_ai.output.messages".into(),
            messages_json(json!([
                {"role": "assistant", "parts": [
                    {"type": "tool_call", "id": "call_shot", "name": "computer.screenshot",
                     "arguments": "{\"display\": 1}"},
                    {"type": "tool_call_response", "id": "call_shot", "response": {"content": [
                        {"type": "text", "text": "Screenshot captured."},
                        {"type": "image", "mimeType": "image/png", "data": raw_base64}
                    ]}},
                    {"type": "text", "content": "The captured charts agree; the missing file was skipped."}
                ], "finish_reason": "stop"}
            ])),
        );
        let span = make_span(
            &trace,
            &span_id,
            None,
            &format!("{}.chat", vendor.provider),
            vendor.service,
            start,
            self.rng.range(800, 9000) * MS,
            "ok",
            attributes,
        );
        self.push(span);
    }

    /// A failed call and its linked retry: error status, `error.type`, and a
    /// `relation: retry-of` link — the non-tree shape agentic traces need.
    fn failure_and_retry(&mut self, index: usize) {
        let vendor = &VENDORS[(index + 1) % VENDORS.len()];
        let session = format!("chat-{:04}", 300 + index);
        let trace = self.id("trace-retry");
        let failed = self.id("span");
        let start = self.advance_rand(15, 200, SEC);
        let failure = *self.rng.pick(&[
            ("RateLimitError", 429, "rate limit exceeded"),
            ("APITimeoutError", 504, "request timed out"),
            ("APIConnectionError", 502, "connection reset"),
            (
                "ContextWindowExceeded",
                400,
                "maximum context length exceeded",
            ),
        ]);

        let mut attributes = self.usage_attributes(vendor, Dialect::Current, 0, 0);
        attributes.remove("gen_ai.usage.input_tokens");
        attributes.remove("gen_ai.usage.output_tokens");
        attributes.remove("llm.cost_usd");
        attributes.insert("gen_ai.conversation.id".into(), json!(session));
        attributes.insert("error.type".into(), json!(failure.0));
        attributes.insert("http.response.status_code".into(), json!(failure.1));
        let mut span = make_span(
            &trace,
            &failed,
            None,
            &format!("{}.chat", vendor.provider),
            vendor.service,
            start,
            self.rng.range(500, 30_000) * MS,
            "error",
            attributes,
        );
        span.events.push(Event {
            name: "exception".into(),
            timestamp_ns: start + 10 * MS,
            attributes: {
                let mut map = Map::new();
                map.insert("exception.type".into(), json!(failure.0));
                map.insert("exception.message".into(), json!(failure.2));
                map
            },
        });
        self.push(span);

        // The retry succeeds and points back at the attempt it replaces.
        let retry = self.id("span");
        let prompt_tokens = self.rng.range(200, 2000);
        let completion_tokens = self.rng.range(50, 400);
        let mut attributes =
            self.usage_attributes(vendor, Dialect::Current, prompt_tokens, completion_tokens);
        attributes.insert("gen_ai.conversation.id".into(), json!(session));
        attributes.insert("gen_ai.response.finish_reason".into(), json!("stop"));
        let retry_start = start + self.rng.range(1, 10) * SEC;
        let mut span = make_span(
            &trace,
            &retry,
            None,
            &format!("{}.chat", vendor.provider),
            vendor.service,
            retry_start,
            self.rng.range(300, 4000) * MS,
            "ok",
            attributes,
        );
        span.links.push(Link {
            trace_id: trace.clone(),
            span_id: failed.clone(),
            attributes: {
                let mut map = Map::new();
                map.insert("relation".into(), json!("retry-of"));
                map.insert("attempt".into(), json!(2));
                map
            },
        });
        self.push(span);

        self.out.annotations.push(Annotation {
            trace_id: trace.clone(),
            span_id: String::new(),
            tenant: String::new(),
            session_id: String::new(),
            experiment_id: None,
            example_id: String::new(),
            name: "incident".into(),
            value: json!(failure.0),
            source: "human:oncall".into(),
            comment: "retried automatically".into(),
            timestamp_ns: retry_start + SEC,
        });
    }

    /// One planner spawning parallel workers that rejoin: wide traces plus
    /// `spawned`/`joins` links.
    fn parallel_fanout(&mut self, index: usize) {
        let vendor = &VENDORS[0];
        let session = format!("swarm-{:04}", index);
        let trace = self.id("trace-swarm");
        let planner = self.id("span");
        let start = self.advance_rand(60, 400, SEC);
        let workers = self.rng.range(4, 12);

        let mut attributes = Map::new();
        attributes.insert("traceloop.span.kind".into(), json!("agent"));
        attributes.insert("traceloop.entity.name".into(), json!("planner"));
        attributes.insert("gen_ai.conversation.id".into(), json!(session));
        let span = make_span(
            &trace,
            &planner,
            None,
            "planner.agent",
            vendor.service,
            start,
            self.rng.range(3, 20) * SEC,
            "ok",
            attributes,
        );
        self.push(span);

        let mut worker_ids = Vec::new();
        for worker in 0..workers {
            let worker_id = self.id("span");
            worker_ids.push(worker_id.clone());
            let prompt_tokens = self.rng.range(200, 1200);
            let completion_tokens = self.rng.range(50, 300);
            let mut attributes =
                self.usage_attributes(vendor, Dialect::Current, prompt_tokens, completion_tokens);
            attributes.insert("traceloop.span.kind".into(), json!("task"));
            attributes.insert(
                "traceloop.entity.name".into(),
                json!(format!("worker_{worker}")),
            );
            attributes.insert("gen_ai.conversation.id".into(), json!(session));
            let mut span = make_span(
                &trace,
                &worker_id,
                Some(&planner),
                &format!("worker_{worker}.task"),
                vendor.service,
                // Workers start together: overlapping bars in the waterfall.
                start + 50 * MS,
                self.rng.range(400, 9000) * MS,
                if self.rng.chance(8) { "error" } else { "ok" },
                attributes,
            );
            span.links.push(Link {
                trace_id: trace.clone(),
                span_id: planner.clone(),
                attributes: {
                    let mut map = Map::new();
                    map.insert("relation".into(), json!("spawned"));
                    map
                },
            });
            self.push(span);
        }

        // The join references every worker it waited on.
        let join = self.id("span");
        let mut attributes = Map::new();
        attributes.insert("traceloop.span.kind".into(), json!("task"));
        attributes.insert("traceloop.entity.name".into(), json!("reduce"));
        attributes.insert("gen_ai.conversation.id".into(), json!(session));
        let mut span = make_span(
            &trace,
            &join,
            Some(&planner),
            "reduce.task",
            vendor.service,
            start + 10 * SEC,
            self.rng.range(100, 900) * MS,
            "ok",
            attributes,
        );
        for worker_id in &worker_ids {
            span.links.push(Link {
                trace_id: trace.clone(),
                span_id: worker_id.clone(),
                attributes: {
                    let mut map = Map::new();
                    map.insert("relation".into(), json!("joins"));
                    map
                },
            });
        }
        self.push(span);
    }

    /// An agent that cannot make progress and does not stop: the shape
    /// attribution exists to name.
    ///
    /// A research agent reflects, searches, and reflects again on what it
    /// found. Its search tool starts failing on the fourth turn and never
    /// recovers, so every later reflection appends the failure to its context
    /// and asks again. Nothing here is a Traza convention: no
    /// `session.outcome`, no `relation: "retry-of"` link, no marker of any
    /// kind saying "this one is the runaway". The session identifies itself
    /// with `gen_ai.conversation.id` and reports usage the way OpenLLMetry
    /// does, which is all a real pipeline emits and therefore all the
    /// analysis is allowed to need.
    ///
    /// Two details are deliberate. The steps are SIBLINGS under one agent
    /// root rather than a parent chain, because that is what LangGraph, the
    /// OpenAI Agents SDK and this file's own `crewai_session` produce — a
    /// detector that only sees nesting would miss every real framework. And
    /// the reflection's context grows every turn while the tool's arguments
    /// barely change, so the two halves of the run are found by two different
    /// signals: growth on one, failure density on the other.
    fn runaway_research_agent(&mut self, index: usize) {
        let vendor = &VENDORS[1];
        let session = format!("runaway-{:04}", 900 + index);
        let trace = self.id("trace-runaway");
        let root = self.id("span");
        let start = self.advance_rand(60, 300, SEC);
        let turns = 9_u64;

        let mut attributes = Map::new();
        attributes.insert("traceloop.span.kind".into(), json!("workflow"));
        attributes.insert("traceloop.workflow.name".into(), json!("research"));
        attributes.insert("gen_ai.conversation.id".into(), json!(session));
        let root_span = make_span(
            &trace,
            &root,
            None,
            "research.workflow",
            vendor.service,
            start,
            turns * 12 * SEC,
            "error",
            attributes,
        );
        self.push(root_span);

        let mut at = start;
        for turn in 0..turns {
            // The reflection: context grows every turn because the last
            // failure is appended to it, which is the runaway's signature and
            // is readable from token counts alone.
            let prompt_tokens = 1_200 + turn * 900;
            let completion_tokens = self.rng.range(80, 260);
            let mut attributes =
                self.usage_attributes(vendor, Dialect::Current, prompt_tokens, completion_tokens);
            attributes.insert("traceloop.span.kind".into(), json!("llm"));
            attributes.insert("gen_ai.conversation.id".into(), json!(session));
            let reflect = self.id("span");
            let think_ns = self.rng.range(900, 2_600) * MS;
            self.push(make_span(
                &trace,
                &reflect,
                Some(&root),
                "agent.reflect",
                vendor.service,
                at,
                think_ns,
                "ok",
                attributes,
            ));
            at += think_ns;

            // The search, which starts failing on turn 3 and stays failed.
            let failing = turn >= 3;
            let mut attributes = Map::new();
            attributes.insert("traceloop.span.kind".into(), json!("tool"));
            attributes.insert("traceloop.entity.name".into(), json!("web_search"));
            attributes.insert("gen_ai.tool.name".into(), json!("web_search"));
            attributes.insert("gen_ai.conversation.id".into(), json!(session));
            attributes.insert(
                "traceloop.entity.input".into(),
                json!(r#"{"query":"quarterly filings 2026"}"#),
            );
            if failing {
                attributes.insert("error.type".into(), json!("SearchBackendUnavailable"));
                attributes.insert("http.response.status_code".into(), json!(503));
            }
            let search = self.id("span");
            let search_ns = self.rng.range(400, 1_500) * MS;
            self.push(make_span(
                &trace,
                &search,
                Some(&root),
                "tool.web_search",
                vendor.service,
                at,
                search_ns,
                if failing { "error" } else { "ok" },
                attributes,
            ));
            at += search_ns;
        }
    }

    /// A large, healthy fan-out: the shape a loop detector must not fire on.
    ///
    /// Forty enrichment calls under one root, in one trace, all with the same
    /// `(service, name)` — which is exactly the input that reaches the
    /// repetition classifier, and exactly what a naive "many identical spans
    /// means a loop" rule reports as a runaway. Everything about it says
    /// ordinary work: the calls overlap in time because they were issued
    /// concurrently, each one carries a different item, the token counts do
    /// not climb, and almost nothing fails.
    ///
    /// It exists so the corpus can prove a negative. Without a healthy
    /// workload that actually reaches the classifier, "the analysis finds no
    /// fault in the seed corpus" is a claim about code that never ran.
    fn bulk_enrichment_fanout(&mut self, index: usize) {
        let vendor = &VENDORS[3 % VENDORS.len()];
        let session = format!("bulk-enrich-{:04}", 700 + index);
        let trace = self.id("trace-enrich");
        let root = self.id("span");
        let start = self.advance_rand(30, 180, SEC);
        let items = 40_u64;

        let mut attributes = Map::new();
        attributes.insert("traceloop.span.kind".into(), json!("workflow"));
        attributes.insert("traceloop.workflow.name".into(), json!("enrich_batch"));
        attributes.insert("session.id".into(), json!(session));
        self.push(make_span(
            &trace,
            &root,
            None,
            "enrich_batch.workflow",
            vendor.service,
            start,
            22 * SEC,
            "ok",
            attributes,
        ));

        for item in 0..items {
            // Concurrent: every call starts within the same short window, so
            // the group overlaps rather than running one after another.
            let began = start + self.rng.range(10, 900) * MS;
            let prompt_tokens = self.rng.range(420, 460);
            let completion_tokens = self.rng.range(30, 60);
            let mut attributes =
                self.usage_attributes(vendor, Dialect::Current, prompt_tokens, completion_tokens);
            attributes.insert("traceloop.span.kind".into(), json!("llm"));
            attributes.insert("session.id".into(), json!(session));
            // A different item each time — this is iteration, not repetition.
            attributes.insert(
                "gen_ai.input.messages".into(),
                messages_json(json!([{
                    "role": "user",
                    "parts": [{"type": "text", "content": format!("Enrich record {item}")}]
                }])),
            );
            // Two of forty fail, which is ordinary flakiness rather than a
            // failing dependency.
            let failed = item % 20 == 7;
            if failed {
                attributes.insert("error.type".into(), json!("UpstreamTimeout"));
            }
            let span_id = self.id("span");
            let duration = self.rng.range(300, 1_800) * MS;
            self.push(make_span(
                &trace,
                &span_id,
                Some(&root),
                "tool.enrich_record",
                vendor.service,
                began,
                duration,
                if failed { "error" } else { "ok" },
                attributes,
            ));
        }
    }

    /// Prompts and completions far past the offload threshold, both as message
    /// attributes and as native `llm.prompt` events.
    fn oversized_payloads(&mut self, index: usize) {
        let vendor = &VENDORS[2];
        let session = format!("bulk-{:04}", index);
        let trace = self.id("trace-bulk");
        let span_id = self.id("span");
        let start = self.advance_rand(30, 200, SEC);
        let big = "The quick brown fox jumps over the lazy dog. "
            .repeat(self.options.big_payload_bytes / 45 + 1);

        let prompt_tokens = self.rng.range(60_000, 190_000);
        let completion_tokens = self.rng.range(500, 4000);
        let mut attributes =
            self.usage_attributes(vendor, Dialect::Current, prompt_tokens, completion_tokens);
        attributes.insert("gen_ai.conversation.id".into(), json!(session));
        attributes.insert(
            "gen_ai.input.messages".into(),
            messages_json(json!([
                {"role": "user", "parts": [{"type": "text", "content": big.clone()}]}
            ])),
        );
        let mut span = make_span(
            &trace,
            &span_id,
            None,
            &format!("{}.chat", vendor.provider),
            vendor.service,
            start,
            self.rng.range(5, 60) * SEC,
            "ok",
            attributes,
        );
        // Native event carriers, which offload independently of attributes.
        span.events.push(Event {
            name: "llm.prompt".into(),
            timestamp_ns: start,
            attributes: {
                let mut map = Map::new();
                map.insert("content".into(), json!(big));
                map
            },
        });
        span.events.push(Event {
            name: "llm.completion".into(),
            timestamp_ns: start + SEC,
            attributes: {
                let mut map = Map::new();
                map.insert(
                    "content".into(),
                    json!("Summarized the corpus in 12 bullets."),
                );
                map
            },
        });
        self.push(span);
    }

    /// A streamed response: time-to-first-token as an event, streaming flag.
    fn streaming_chat(&mut self, index: usize) {
        let vendor = &VENDORS[(index + 3) % VENDORS.len()];
        let session = format!("chat-{:04}", 600 + index);
        let trace = self.id("trace-stream");
        let span_id = self.id("span");
        let start = self.advance_rand(10, 150, SEC);
        let prompt_tokens = self.rng.range(100, 1500);
        let completion_tokens = self.rng.range(100, 2000);
        let mut attributes =
            self.usage_attributes(vendor, Dialect::Current, prompt_tokens, completion_tokens);
        attributes.insert("gen_ai.conversation.id".into(), json!(session));
        attributes.insert("llm.is_streaming".into(), json!(true));
        attributes.insert("gen_ai.response.finish_reason".into(), json!("stop"));
        let ttft = self.rng.range(80, 900) * MS;
        let mut span = make_span(
            &trace,
            &span_id,
            None,
            &format!("{}.chat", vendor.provider),
            vendor.service,
            start,
            ttft + self.rng.range(500, 12_000) * MS,
            "ok",
            attributes,
        );
        span.events.push(Event {
            name: "gen_ai.content.completion.chunk".into(),
            timestamp_ns: start + ttft,
            attributes: {
                let mut map = Map::new();
                map.insert("index".into(), json!(0));
                map
            },
        });
        self.push(span);
    }

    /// Ordinary service telemetry with no GenAI attributes at all: it must be
    /// stored and searchable, and must never be counted as an LLM call.
    fn plain_service_traffic(&mut self, index: usize) {
        let trace = self.id("trace-http");
        let root = self.id("span");
        let start = self.advance_rand(1, 40, SEC);
        let route = *self.rng.pick(&[
            "/api/orders",
            "/api/orders/{id}",
            "/healthz",
            "/api/users/{id}/sessions",
        ]);
        let method = *self.rng.pick(&["GET", "POST", "PUT", "DELETE"]);
        let failed = self.rng.chance(7);
        let status_code = if failed { 500 } else { 200 };

        let mut attributes = Map::new();
        attributes.insert("http.request.method".into(), json!(method));
        attributes.insert("url.path".into(), json!(route));
        attributes.insert("http.response.status_code".into(), json!(status_code));
        attributes.insert("server.address".into(), json!("api.internal"));
        attributes.insert(
            "region".into(),
            json!(*self.rng.pick(&["us-east", "eu-west", "ap-south"])),
        );
        let http_ms = self.rng.range(2, 900) * MS;
        let span = make_span(
            &trace,
            &root,
            None,
            &format!("{method} {route}"),
            "checkout",
            start,
            http_ms,
            if failed { "error" } else { "ok" },
            attributes,
        );
        self.push(span);

        // A database child span.
        let db = self.id("span");
        let mut attributes = Map::new();
        attributes.insert("db.system".into(), json!("postgresql"));
        attributes.insert("db.namespace".into(), json!("orders"));
        attributes.insert(
            "db.statement".into(),
            json!("select id, total from orders where customer_id = $1"),
        );
        let span = make_span(
            &trace,
            &db,
            Some(&root),
            "SELECT orders",
            "checkout",
            start + MS,
            self.rng.range(1, 300) * MS,
            "ok",
            attributes,
        );
        self.push(span);

        let _ = index;
    }

    /// OpenAI-shaped: response id and system fingerprint, OpenAI tool-call
    /// arguments as a JSON *string* (as the API returns them), and a
    /// structured-output turn whose answer is JSON.
    fn openai_session(&mut self, index: usize) {
        let vendor = &VENDORS[0];
        let session = format!("openai-{:04}", index);
        let trace = self.id("trace-openai");
        let root = self.id("span");
        let start = self.advance_rand(20, 200, SEC);

        let prompt_tokens = self.rng.range(400, 2000);
        let completion_tokens = self.rng.range(60, 400);
        let mut attributes =
            self.usage_attributes(vendor, Dialect::Current, prompt_tokens, completion_tokens);
        attributes.insert("gen_ai.conversation.id".into(), json!(session));
        attributes.insert(
            "gen_ai.response.id".into(),
            json!(format!("chatcmpl-{:012x}", self.rng.next_u64())),
        );
        attributes.insert(
            "gen_ai.openai.response.system_fingerprint".into(),
            json!("fp_44709d6fcb"),
        );
        attributes.insert("gen_ai.output.type".into(), json!("json"));
        attributes.insert("gen_ai.response.finish_reason".into(), json!("tool_calls"));
        attributes.insert(
            "gen_ai.input.messages".into(),
            messages_json(json!([
                {"role": "system", "parts": [{"type": "text", "content": "Classify the ticket and return JSON matching the schema."}]},
                {"role": "user", "parts": [{"type": "text", "content": "Order A-441902 still hasn't shipped and I've been on hold for 40 minutes. I want a refund."}]}
            ])),
        );
        attributes.insert(
            "gen_ai.output.messages".into(),
            messages_json(json!([
                {"role": "assistant", "parts": [{
                    "type": "tool_call",
                    "id": "call_9sLk2",
                    "name": "lookup_order",
                    // OpenAI returns arguments as a JSON string, not an object.
                    "arguments": "{\"order_id\":\"A-441902\"}"
                }], "finish_reason": "tool_calls"}
            ])),
        );
        let span = make_span(
            &trace,
            &root,
            None,
            "openai.chat",
            vendor.service,
            start,
            self.rng.range(300, 1800) * MS,
            "ok",
            attributes,
        );
        self.push(span);

        // The structured-output answer.
        let answer = self.id("span");
        let prompt_tokens = self.rng.range(600, 2400);
        let completion_tokens = self.rng.range(80, 300);
        let mut attributes =
            self.usage_attributes(vendor, Dialect::Current, prompt_tokens, completion_tokens);
        attributes.insert("gen_ai.conversation.id".into(), json!(session));
        attributes.insert("gen_ai.output.type".into(), json!("json"));
        attributes.insert("gen_ai.response.finish_reason".into(), json!("stop"));
        attributes.insert(
            "gen_ai.input.messages".into(),
            messages_json(json!([
                {"role": "tool", "parts": [{"type": "tool_call_response", "id": "call_9sLk2", "result": "{\"status\":\"delayed\",\"eta\":\"2026-01-09\"}"}]}
            ])),
        );
        attributes.insert(
            "gen_ai.output.messages".into(),
            messages_json(json!([
                {"role": "assistant", "parts": [{"type": "text", "content": JSON_ANSWER}], "finish_reason": "stop"}
            ])),
        );
        let span = make_span(
            &trace,
            &answer,
            Some(&root),
            "openai.chat",
            vendor.service,
            start + 2 * SEC,
            self.rng.range(300, 1500) * MS,
            "ok",
            attributes,
        );
        self.push(span);
    }

    /// Anthropic-shaped: prompt-cache token counters and a markdown answer.
    fn anthropic_session(&mut self, index: usize) {
        let vendor = &VENDORS[2];
        let session = format!("anthropic-{:04}", index);
        let trace = self.id("trace-anthropic");
        let span_id = self.id("span");
        let start = self.advance_rand(20, 200, SEC);

        let prompt_tokens = self.rng.range(2000, 9000);
        let completion_tokens = self.rng.range(200, 1200);
        let mut attributes =
            self.usage_attributes(vendor, Dialect::Current, prompt_tokens, completion_tokens);
        attributes.insert("gen_ai.conversation.id".into(), json!(session));
        // Anthropic's cache counters, which OpenLLMetry records verbatim.
        attributes.insert(
            "gen_ai.usage.cache_creation_input_tokens".into(),
            json!(self.rng.range(0, 3000)),
        );
        attributes.insert(
            "gen_ai.usage.cache_read_input_tokens".into(),
            json!(self.rng.range(0, 12000)),
        );
        attributes.insert("gen_ai.response.stop_reason".into(), json!("end_turn"));
        attributes.insert("gen_ai.response.finish_reason".into(), json!("end_turn"));
        attributes.insert(
            "gen_ai.input.messages".into(),
            messages_json(json!([
                {"role": "user", "parts": [{"type": "text", "content": "Summarize the Q4 report for the board, with a table by region."}]}
            ])),
        );
        attributes.insert(
            "gen_ai.output.messages".into(),
            messages_json(json!([
                {"role": "assistant", "parts": [{"type": "text", "content": MARKDOWN_ANSWER}], "finish_reason": "end_turn"}
            ])),
        );
        let span = make_span(
            &trace,
            &span_id,
            None,
            "anthropic.chat",
            vendor.service,
            start,
            self.rng.range(800, 6000) * MS,
            "ok",
            attributes,
        );
        self.push(span);
    }

    /// A LangGraph run: a graph root carrying its node/edge topology, then one
    /// span per node, some of which are LLM calls and some plain state work.
    fn langgraph_session(&mut self, index: usize) {
        let vendor = &VENDORS[4];
        let session = format!("graph-{:04}", index);
        let trace = self.id("trace-langgraph");
        let root = self.id("span");
        let start = self.advance_rand(30, 300, SEC);
        let nodes = ["retrieve", "grade_documents", "generate", "reflect"];

        let mut attributes = Map::new();
        attributes.insert("traceloop.span.kind".into(), json!("workflow"));
        attributes.insert("traceloop.workflow.name".into(), json!("LangGraph"));
        attributes.insert("traceloop.entity.name".into(), json!("self_rag_graph"));
        attributes.insert("gen_ai.conversation.id".into(), json!(session));
        attributes.insert("gen_ai.workflow.nodes".into(), json!(nodes));
        attributes.insert(
            "gen_ai.workflow.edges".into(),
            json!([
                ["retrieve", "grade_documents"],
                ["grade_documents", "generate"],
                ["generate", "reflect"],
                ["reflect", "generate"]
            ]),
        );
        attributes.insert("framework".into(), json!("langgraph"));
        let span = make_span(
            &trace,
            &root,
            None,
            "self_rag_graph.workflow",
            vendor.service,
            start,
            self.rng.range(3, 25) * SEC,
            "ok",
            attributes,
        );
        self.push(span);

        let mut previous = root.clone();
        for (step, node) in nodes.iter().enumerate() {
            let node_span = self.id("span");
            let is_llm = matches!(*node, "generate" | "reflect" | "grade_documents");
            let mut attributes = if is_llm {
                let prompt_tokens = self.rng.range(500, 4000);
                let completion_tokens = self.rng.range(60, 600);
                self.usage_attributes(vendor, Dialect::Current, prompt_tokens, completion_tokens)
            } else {
                Map::new()
            };
            attributes.insert(
                "traceloop.span.kind".into(),
                json!(if is_llm { "llm" } else { "task" }),
            );
            attributes.insert("traceloop.entity.name".into(), json!(*node));
            attributes.insert("gen_ai.task.name".into(), json!(*node));
            attributes.insert("gen_ai.task.id".into(), json!(format!("{node}-{step}")));
            attributes.insert("gen_ai.task.parent.id".into(), json!("self_rag_graph"));
            attributes.insert("gen_ai.conversation.id".into(), json!(session));
            attributes.insert("framework".into(), json!("langgraph"));
            attributes.insert(
                "traceloop.entity.input".into(),
                json!(format!(
                    "{{\"question\":\"What changed in Q4?\",\"step\":{step}}}"
                )),
            );
            if is_llm {
                attributes.insert(
                    "gen_ai.output.messages".into(),
                    messages_json(json!([
                        {"role": "assistant", "parts": [{"type": "text", "content": format!("Node `{node}` decided: continue.")}], "finish_reason": "stop"}
                    ])),
                );
            }
            let span = make_span(
                &trace,
                &node_span,
                Some(&previous),
                &format!("{node}.langgraph"),
                vendor.service,
                start + (step as u64 + 1) * SEC,
                self.rng.range(200, 4000) * MS,
                "ok",
                attributes,
            );
            self.push(span);
            previous = node_span;
        }
    }

    /// A CrewAI run: a crew root, agents with roles, and their delegated tasks.
    fn crewai_session(&mut self, index: usize) {
        let vendor = &VENDORS[1];
        let session = format!("crew-{:04}", index);
        let trace = self.id("trace-crewai");
        let root = self.id("span");
        let start = self.advance_rand(30, 300, SEC);
        let agents: &[(&str, &str)] = &[
            ("researcher", "Find primary sources on the topic"),
            ("analyst", "Turn sources into a numbers-first brief"),
            ("writer", "Write the final memo in plain language"),
        ];

        let mut attributes = Map::new();
        attributes.insert("traceloop.span.kind".into(), json!("workflow"));
        attributes.insert("traceloop.workflow.name".into(), json!("Crew"));
        attributes.insert(
            "traceloop.entity.name".into(),
            json!("market_research_crew"),
        );
        attributes.insert("gen_ai.conversation.id".into(), json!(session));
        attributes.insert("framework".into(), json!("crewai"));
        attributes.insert("crewai.crew.process".into(), json!("sequential"));
        attributes.insert(
            "crewai.crew.agents".into(),
            json!(["researcher", "analyst", "writer"]),
        );
        let span = make_span(
            &trace,
            &root,
            None,
            "market_research_crew.workflow",
            vendor.service,
            start,
            self.rng.range(10, 60) * SEC,
            "ok",
            attributes,
        );
        self.push(span);

        for (step, (role, goal)) in agents.iter().enumerate() {
            let agent_span = self.id("span");
            let mut attributes = Map::new();
            attributes.insert("traceloop.span.kind".into(), json!("agent"));
            attributes.insert("traceloop.entity.name".into(), json!(*role));
            attributes.insert("gen_ai.agent.name".into(), json!(*role));
            attributes.insert("gen_ai.agent.description".into(), json!(*goal));
            attributes.insert("gen_ai.conversation.id".into(), json!(session));
            attributes.insert("framework".into(), json!("crewai"));
            let agent_start = start + (step as u64 * 4 + 1) * SEC;
            let span = make_span(
                &trace,
                &agent_span,
                Some(&root),
                &format!("{role}.agent"),
                vendor.service,
                agent_start,
                self.rng.range(2, 12) * SEC,
                "ok",
                attributes,
            );
            self.push(span);

            // Each agent runs one task, which makes one LLM call.
            let task_span = self.id("span");
            let prompt_tokens = self.rng.range(300, 2500);
            let completion_tokens = self.rng.range(80, 700);
            let mut attributes =
                self.usage_attributes(vendor, Dialect::Current, prompt_tokens, completion_tokens);
            attributes.insert("traceloop.span.kind".into(), json!("llm"));
            attributes.insert("gen_ai.conversation.id".into(), json!(session));
            attributes.insert("framework".into(), json!("crewai"));
            attributes.insert("gen_ai.agent.name".into(), json!(*role));
            attributes.insert(
                "gen_ai.input.messages".into(),
                messages_json(json!([
                    {"role": "system", "parts": [{"type": "text", "content": format!("You are the {role}. {goal}.")}]},
                    {"role": "user", "parts": [{"type": "text", "content": "Produce your section of the market brief."}]}
                ])),
            );
            attributes.insert(
                "gen_ai.output.messages".into(),
                messages_json(json!([
                    {"role": "assistant", "parts": [{"type": "text", "content": if step == 2 { MARKDOWN_ANSWER } else { "Section drafted; handing off." }}], "finish_reason": "stop"}
                ])),
            );
            let span = make_span(
                &trace,
                &task_span,
                Some(&agent_span),
                &format!("{role}_task.task"),
                vendor.service,
                agent_start + 200 * MS,
                self.rng.range(500, 8000) * MS,
                "ok",
                attributes,
            );
            self.push(span);
        }
    }

    /// Turns whose OUTPUT is media: an image the model generated, speech it
    /// synthesized, and a rendered video. Inline data URIs, so a dashboard can
    /// actually display them.
    fn generated_media_session(&mut self, index: usize) {
        let session = format!("studio-{:04}", index);
        let png = demo_image_png();
        let svg = demo_image_svg();
        let gif = demo_animation_gif();
        let wav = demo_audio_wav();

        // 1) Image generation.
        let vendor = &VENDORS[0];
        let trace = self.id("trace-imagegen");
        let span_id = self.id("span");
        let start = self.advance_rand(20, 180, SEC);
        let mut attributes = Map::new();
        attributes.insert("gen_ai.provider.name".into(), json!("openai"));
        attributes.insert("gen_ai.operation.name".into(), json!("image.generation"));
        attributes.insert("gen_ai.request.model".into(), json!("gpt-image-1"));
        attributes.insert(
            "gen_ai.usage.input_tokens".into(),
            json!(self.rng.range(20, 120)),
        );
        attributes.insert("gen_ai.output.type".into(), json!("image"));
        attributes.insert("gen_ai.conversation.id".into(), json!(session));
        attributes.insert("llm.cost_usd".into(), json!(0.04));
        attributes.insert(
            "gen_ai.input.messages".into(),
            messages_json(json!([
                {"role": "user", "parts": [{"type": "text", "content": "Chart revenue by month as bars, paper background."}]}
            ])),
        );
        attributes.insert(
            "gen_ai.output.messages".into(),
            messages_json(json!([
                {"role": "assistant", "parts": [
                    {"type": "text", "content": "Here is the revenue chart, as a raster image and as vector art."},
                    {"type": "image", "mime_type": "image/png", "filename": "revenue.png",
                     "size_bytes": png.len(), "data": png},
                    {"type": "image", "mime_type": "image/svg+xml", "filename": "revenue.svg",
                     "size_bytes": svg.len(), "data": svg}
                ], "finish_reason": "stop"}
            ])),
        );
        let span = make_span(
            &trace,
            &span_id,
            None,
            "openai.images.generate",
            vendor.service,
            start,
            self.rng.range(2, 20) * SEC,
            "ok",
            attributes,
        );
        self.push(span);

        // 2) Text to speech.
        let trace = self.id("trace-tts");
        let span_id = self.id("span");
        let start = self.advance_rand(10, 90, SEC);
        let mut attributes = Map::new();
        attributes.insert("gen_ai.provider.name".into(), json!("openai"));
        attributes.insert("gen_ai.operation.name".into(), json!("audio.speech"));
        attributes.insert("gen_ai.request.model".into(), json!("gpt-4o-mini-tts"));
        attributes.insert("gen_ai.output.type".into(), json!("speech"));
        attributes.insert(
            "gen_ai.usage.input_tokens".into(),
            json!(self.rng.range(10, 80)),
        );
        attributes.insert("gen_ai.conversation.id".into(), json!(session));
        attributes.insert("llm.cost_usd".into(), json!(0.015));
        attributes.insert(
            "gen_ai.input.messages".into(),
            messages_json(json!([
                {"role": "user", "parts": [{"type": "text", "content": "Read the summary aloud."}]}
            ])),
        );
        attributes.insert(
            "gen_ai.output.messages".into(),
            messages_json(json!([
                {"role": "assistant", "parts": [
                    {"type": "text", "content": "Here is the audio summary."},
                    {"type": "audio", "mime_type": "audio/wav", "filename": "summary.wav",
                     "size_bytes": wav.len(), "data": wav}
                ], "finish_reason": "stop"}
            ])),
        );
        let span = make_span(
            &trace,
            &span_id,
            None,
            "openai.audio.speech",
            vendor.service,
            start,
            self.rng.range(1, 8) * SEC,
            "ok",
            attributes,
        );
        self.push(span);

        // 3) Video generation — too large to inline, referenced by URL with a
        //    poster image, the shape a real pipeline produces.
        let trace = self.id("trace-videogen");
        let span_id = self.id("span");
        let start = self.advance_rand(30, 200, SEC);
        let mut attributes = Map::new();
        attributes.insert("gen_ai.provider.name".into(), json!("gcp.vertex_ai"));
        attributes.insert("gen_ai.operation.name".into(), json!("video.generation"));
        attributes.insert("gen_ai.request.model".into(), json!("veo-3"));
        attributes.insert("gen_ai.output.type".into(), json!("video"));
        attributes.insert("gen_ai.conversation.id".into(), json!(session));
        attributes.insert("llm.cost_usd".into(), json!(1.20));
        attributes.insert(
            "gen_ai.input.messages".into(),
            messages_json(json!([
                {"role": "user", "parts": [{"type": "text", "content": "Animate the revenue bars growing, 6 frames."}]}
            ])),
        );
        attributes.insert(
            "gen_ai.output.messages".into(),
            messages_json(json!([
                {"role": "assistant", "parts": [
                    {"type": "text", "content": "Rendered 6 frames; the looping preview is below, the full render is in object storage."},
                    {"type": "image", "mime_type": "image/gif", "filename": "preview.gif",
                     "size_bytes": gif.len(), "data": gif},
                    {"type": "video", "mime_type": "video/mp4", "filename": "render.mp4",
                     "size_bytes": 8_412_233, "uri": "s3://traza-demo/renders/render.mp4"}
                ], "finish_reason": "stop"}
            ])),
        );
        let span = make_span(
            &trace,
            &span_id,
            None,
            "vertex_ai.video.generate",
            "studio-agent",
            start,
            self.rng.range(20, 120) * SEC,
            "ok",
            attributes,
        );
        self.push(span);
    }

    /// One session whose answers cover every text shape the renderer must
    /// handle: markdown, fenced code, JSON, and plain prose.
    fn content_formats_session(&mut self, index: usize) {
        let vendor = &VENDORS[2];
        let session = format!("formats-{:04}", index);
        let answers: &[(&str, &str)] = &[
            ("markdown", MARKDOWN_ANSWER),
            ("code", CODE_ANSWER),
            ("json", JSON_ANSWER),
            (
                "plain",
                "No table needed — revenue is up 12% and margin is flat. I'd confirm EMEA with finance before the board sees it.",
            ),
        ];
        for (kind, answer) in answers {
            let trace = self.id("trace-format");
            let span_id = self.id("span");
            let start = self.advance_rand(10, 120, SEC);
            let prompt_tokens = self.rng.range(200, 1500);
            let completion_tokens = self.rng.range(80, 900);
            let mut attributes =
                self.usage_attributes(vendor, Dialect::Current, prompt_tokens, completion_tokens);
            attributes.insert("gen_ai.conversation.id".into(), json!(session));
            attributes.insert("gen_ai.response.finish_reason".into(), json!("stop"));
            attributes.insert("response.format".into(), json!(*kind));
            attributes.insert(
                "gen_ai.input.messages".into(),
                messages_json(json!([
                    {"role": "user", "parts": [{"type": "text", "content": format!("Answer as {kind}.")}]}
                ])),
            );
            attributes.insert(
                "gen_ai.output.messages".into(),
                messages_json(json!([
                    {"role": "assistant", "parts": [{"type": "text", "content": *answer}], "finish_reason": "stop"}
                ])),
            );
            let span = make_span(
                &trace,
                &span_id,
                None,
                "anthropic.chat",
                vendor.service,
                start,
                self.rng.range(400, 6000) * MS,
                "ok",
                attributes,
            );
            self.push(span);
        }
    }
}

/// Builds a span. A free function, not a method, so a caller can compute
/// random durations from the generator's RNG in the argument list.
#[allow(clippy::too_many_arguments)]
fn make_span(
    trace_id: &str,
    span_id: &str,
    parent: Option<&str>,
    name: &str,
    service: &str,
    start_ns: u64,
    duration_ns: u64,
    status: &str,
    attributes: Map<String, Value>,
) -> Span {
    Span {
        trace_id: trace_id.to_owned(),
        span_id: span_id.to_owned(),
        tenant: String::new(),
        parent_span_id: parent.map(str::to_owned),
        name: name.to_owned(),
        start_time_ns: start_ns,
        end_time_ns: start_ns + duration_ns,
        status: status.to_owned(),
        service: service.to_owned(),
        attributes,
        events: Vec::new(),
        links: Vec::new(),
        extra: Map::new(),
    }
}

/// A JSON-encoded `gen_ai.input.messages` / `gen_ai.output.messages` value,
/// matching how OpenLLMetry emits them (a JSON string, not a structure).
fn messages_json(value: Value) -> Value {
    Value::String(value.to_string())
}

/// Rounds a metered cost to the nearest hundredth of a cent, so summed costs
/// stay readable instead of trailing float noise.
fn round_cents(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn generation_is_deterministic() {
        let options = SeedOptions::default();
        let first = corpus(&options);
        let second = corpus(&options);
        assert_eq!(first.spans, second.spans, "same options, same spans");
        assert_eq!(first.spans.len(), second.spans.len());
        assert!(!first.is_empty());
    }

    #[test]
    fn a_different_seed_changes_the_corpus() {
        let a = corpus(&SeedOptions::default());
        let b = corpus(&SeedOptions {
            seed: 99,
            ..SeedOptions::default()
        });
        assert_ne!(a.spans, b.spans);
    }

    #[test]
    fn span_identity_is_unique_across_the_corpus() {
        // Duplicate (trace_id, span_id) pairs would silently upsert and make
        // every count in the scenario tests wrong.
        let generated = corpus(&SeedOptions {
            scale: 3,
            ..SeedOptions::default()
        });
        let mut keys = HashSet::new();
        for span in &generated.spans {
            assert!(
                keys.insert((span.trace_id.clone(), span.span_id.clone())),
                "duplicate span identity: {}/{}",
                span.trace_id,
                span.span_id
            );
            assert!(span.end_time_ns >= span.start_time_ns, "negative duration");
            assert!(!span.name.is_empty() && !span.service.is_empty());
        }
    }

    #[test]
    fn scale_grows_the_corpus() {
        let small = corpus(&SeedOptions::default()).len();
        let large = corpus(&SeedOptions {
            scale: 4,
            ..SeedOptions::default()
        })
        .len();
        assert!(
            large > small * 2,
            "scale 4 should dwarf scale 1: {small} vs {large}"
        );
    }

    #[test]
    fn parents_and_links_reference_spans_in_the_same_corpus() {
        let generated = corpus(&SeedOptions::default());
        let ids: HashSet<(&str, &str)> = generated
            .spans
            .iter()
            .map(|span| (span.trace_id.as_str(), span.span_id.as_str()))
            .collect();
        for span in &generated.spans {
            if let Some(parent) = &span.parent_span_id {
                assert!(
                    ids.contains(&(span.trace_id.as_str(), parent.as_str())),
                    "dangling parent {parent} in {}",
                    span.trace_id
                );
            }
            for link in &span.links {
                assert!(
                    ids.contains(&(link.trace_id.as_str(), link.span_id.as_str())),
                    "dangling link {}/{}",
                    link.trace_id,
                    link.span_id
                );
            }
        }
    }
}
