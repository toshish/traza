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
/// corpus reproducible without pulling in `rand` — and the crate's own
/// randomized tests with it.
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed.wrapping_add(0x9e37_79b9_7f4a_7c15))
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `lo..=hi`.
    pub(crate) fn range(&mut self, lo: u64, hi: u64) -> u64 {
        if hi <= lo {
            return lo;
        }
        lo + self.next_u64() % (hi - lo + 1)
    }

    pub(crate) fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[(self.next_u64() % items.len() as u64) as usize]
    }

    /// True `percent` of the time.
    pub(crate) fn chance(&mut self, percent: u64) -> bool {
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
    /// Per-1K costs of prompt tokens read from the cache, and written to it.
    cache_read_per_1k: f64,
    cache_write_per_1k: f64,
}

impl Vendor {
    /// The cost of one call, cache reads and writes priced apart from the
    /// rest of the prompt, as Anthropic counts them.
    fn cost(&self, prompt: u64, completion: u64, cache_read: u64, cache_write: u64) -> f64 {
        (prompt as f64 * self.prompt_per_1k
            + completion as f64 * self.completion_per_1k
            + cache_read as f64 * self.cache_read_per_1k
            + cache_write as f64 * self.cache_write_per_1k)
            / 1000.0
    }
}

/// List prices as the vendors publish them (2026-10-18). Bedrock, Vertex AI
/// and Azure are metered at the model maker's own list price, not their
/// platform's.
const VENDORS: &[Vendor] = &[
    Vendor {
        provider: "openai",
        model: "gpt-6.1-sol",
        service: "support-agent",
        prompt_per_1k: 0.002,
        completion_per_1k: 0.010,
        cache_read_per_1k: 0.0001,
        cache_write_per_1k: 0.0025,
    },
    Vendor {
        provider: "openai",
        model: "gpt-6-luna",
        service: "support-agent",
        prompt_per_1k: 0.0001,
        completion_per_1k: 0.0005,
        cache_read_per_1k: 0.000_01,
        cache_write_per_1k: 0.000_125,
    },
    Vendor {
        provider: "anthropic",
        model: "claude-sonnet-5-5",
        service: "research-agent",
        prompt_per_1k: 0.002,
        completion_per_1k: 0.010,
        cache_read_per_1k: 0.0001,
        cache_write_per_1k: 0.0025,
    },
    Vendor {
        provider: "aws.bedrock",
        model: "anthropic.claude-haiku-5-5",
        service: "batch-worker",
        prompt_per_1k: 0.0001,
        completion_per_1k: 0.0005,
        cache_read_per_1k: 0.000_01,
        cache_write_per_1k: 0.000_125,
    },
    Vendor {
        provider: "gcp.vertex_ai",
        model: "gemini-3.8-flash",
        service: "research-agent",
        // Google's introductory price, until 2026-12-31. It bills cache
        // storage by the hour rather than per token written.
        prompt_per_1k: 0.000_75,
        completion_per_1k: 0.003_75,
        cache_read_per_1k: 0.000_075,
        cache_write_per_1k: 0.0,
    },
    Vendor {
        provider: "azure.ai.openai",
        model: "gpt-6.1-sol",
        service: "checkout-copilot",
        prompt_per_1k: 0.002,
        completion_per_1k: 0.010,
        cache_read_per_1k: 0.0001,
        cache_write_per_1k: 0.0025,
    },
    Vendor {
        provider: "ollama",
        model: "qwen3.6",
        service: "edge-agent",
        prompt_per_1k: 0.0,
        completion_per_1k: 0.0,
        cache_read_per_1k: 0.0,
        cache_write_per_1k: 0.0,
    },
    Vendor {
        provider: "cohere",
        model: "command-a-plus-05-2026",
        service: "batch-worker",
        // Synthetic: Cohere's price for this model is not verified here.
        prompt_per_1k: 0.0025,
        completion_per_1k: 0.010,
        cache_read_per_1k: 0.0,
        cache_write_per_1k: 0.0,
    },
];

/// Agent tools: name, the call's arguments, and what it returned.
const TOOLS: &[(&str, &str, &str)] = &[
    (
        "web_search",
        "{\"query\":\"OpenTelemetry GenAI semantic conventions agent spans\"}",
        "[{\"title\":\"Semantic conventions for generative AI agent spans\",\"url\":\"https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-agent-spans/\"}]",
    ),
    (
        "mcp__github__create_pull_request",
        "{\"owner\":\"acme\",\"repo\":\"shop\",\"title\":\"Floor the checkout total at zero\",\"head\":\"fix/checkout-total\",\"base\":\"main\"}",
        "{\"number\":4183,\"html_url\":\"https://github.com/acme/shop/pull/4183\"}",
    ),
    (
        "Read",
        "{\"file_path\":\"/srv/shop/src/checkout/total.ts\"}",
        "5\t  return cart.subtotal - discount;",
    ),
    (
        "Bash",
        "{\"command\":\"npm test -- tests/e2e/checkout.spec.ts\"}",
        "Tests  1 failed | 5 passed (6)",
    ),
    (
        "mcp__playwright__browser_navigate",
        "{\"url\":\"https://staging.shop.example/checkout\"}",
        "Navigated to https://staging.shop.example/checkout (title: Checkout)",
    ),
    (
        "Edit",
        "{\"file_path\":\"/srv/shop/src/checkout/total.ts\",\"old_string\":\"return cart.subtotal - discount;\",\"new_string\":\"return Math.max(cart.subtotal - discount, 0);\"}",
        "The file has been updated.",
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
        let cost = vendor.cost(prompt_tokens, completion_tokens, 0, 0);
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
        // Claude models since 4.7 refuse a temperature other than their
        // default, so a Claude call sends none. It is drawn either way, so
        // no later draw depends on which vendor took the call.
        let temperature = *self.rng.pick(&[0.0_f64, 0.2, 0.7, 1.0]);
        if !vendor.model.contains("claude") {
            attributes.insert("gen_ai.request.temperature".into(), json!(temperature));
        }
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
        let (tool_name, tool_args, tool_result) = *self.rng.pick(TOOLS);

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
                {"role": "system", "parts": [{"type": "text", "content": "You are an engineering agent. Use tools when needed."}]},
                {"role": "user", "parts": [{"type": "text", "content": "Can you look into this for me?"}]}
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
                "{\"error\":\"upstream timeout after 5000ms\"}".to_owned()
            } else {
                json!({ "result": tool_result }).to_string()
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
                {"role": "user", "parts": [{"type": "text", "content": "Can you look into this for me?"}]},
                {"role": "tool", "parts": [{
                    "type": "tool_call_response",
                    "id": format!("call_{index:04}"),
                    "result": if tool_failed { "error: upstream timeout" } else { tool_result }
                }]}
            ])),
        );
        let answer_text = if tool_failed {
            format!("`{tool_name}` timed out before it answered, so nothing was done.")
        } else {
            format!("Here is what `{tool_name}` returned: {tool_result}")
        };
        attributes.insert(
            "gen_ai.output.messages".into(),
            messages_json(json!([
                {"role": "assistant", "parts": [{"type": "text", "content": answer_text}], "finish_reason": "stop"}
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
        // Anthropic's cache counters, in the dotted spelling current
        // OpenLLMetry emits. The underscore forms an older exporter sends are
        // recognized too (see `crate::semconv`); the corpus models what a
        // current pipeline actually sends. The metered cost counts them.
        let cache_write = self.rng.range(0, 3000);
        let cache_read = self.rng.range(0, 12000);
        attributes.insert(
            "gen_ai.usage.cache_creation.input_tokens".into(),
            json!(cache_write),
        );
        attributes.insert(
            "gen_ai.usage.cache_read.input_tokens".into(),
            json!(cache_read),
        );
        attributes.insert(
            "llm.cost_usd".into(),
            json!(round_cents(vendor.cost(
                prompt_tokens,
                completion_tokens,
                cache_read,
                cache_write
            ))),
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

// ------------------------------------------------------- behaviour corpus

/// How [`behaviour_corpus`] lays its runs out.
#[derive(Clone, Debug)]
pub struct BehaviourOptions {
    /// "Now", Unix nanoseconds. Every completed run ends more than an hour
    /// before it — past the idle window, so its outcome is settled — and
    /// every live run's last span ends within the last minute.
    pub now_ns: u64,
    /// The tenant every span is stamped with; empty is the default tenant.
    pub tenant: String,
    /// Multiplier on every branch's run counts (and outcome counts).
    pub scale: usize,
    /// Prepended to every run id, so several corpora can share a store.
    pub prefix: String,
    /// Whether to add the live demonstration runs (`live-doomed-*`,
    /// `live-healthy-0`, `live-novel-0`, `live-early-0`, and the coding
    /// agent's `live-ci-doomed-0`, `live-ci-healthy-0`, `live-ci-novel-0`),
    /// still running.
    pub live: bool,
    /// PRNG seed for step durations and the coding agent's token counts.
    /// Outcomes are exact, never random.
    pub seed: u64,
}

impl Default for BehaviourOptions {
    fn default() -> Self {
        Self {
            now_ns: SeedOptions::default().start_time_ns + 30 * 86_400 * SEC,
            tenant: String::new(),
            scale: 1,
            prefix: String::new(),
            live: false,
            seed: 0x0fa7e,
        }
    }
}

/// One generated run and the truth about it, for tests to assert against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BehaviourRun {
    /// The run id (its `session.id`).
    pub run: String,
    /// The cohort it belongs to.
    pub agent: &'static str,
    /// The behaviour branch it follows.
    pub branch: &'static str,
    /// `success`, `failure`, `abandoned`, or `live`.
    pub outcome: &'static str,
}

/// A behaviour corpus: spans, and the ground truth behind them.
#[derive(Clone, Debug, Default)]
pub struct BehaviourCorpus {
    /// Every span, in generation order.
    pub spans: Vec<Span>,
    /// Every run, in generation order.
    pub runs: Vec<BehaviourRun>,
}

/// One step of a branch: kind, name, consecutive repeats. A tool step of
/// the coding agent names its call the way Claude Code's permission rules
/// do: `Bash(npm test)` is the `Bash` tool, run on `npm test`.
type BranchStep = (&'static str, &'static str, usize);

/// A behaviour branch: who runs it, what it does, and how it ends.
struct Branch {
    agent: &'static str,
    name: &'static str,
    steps: &'static [BranchStep],
    runs: usize,
    success: usize,
    abandoned: usize,
    goal: Option<&'static str>,
}

const GPT: BranchStep = ("llm", "gpt-6.1-sol", 1);
const SONNET: BranchStep = ("llm", "claude-sonnet-5-5", 1);
const HAIKU: BranchStep = ("llm", "claude-haiku-5-5", 1);

/// The coding agent: Claude Code, run headless in a CI job to make the
/// job's failing test pass.
const CODER: &str = "ci-fix";

/// The coding agent's calls. Every test run is the job's failing test.
const TEST: BranchStep = ("tool", "Bash(npm test -- tests/e2e/checkout.spec.ts)", 1);
const READ: BranchStep = ("tool", "Read(src/checkout/total.ts)", 1);
const EDIT: BranchStep = ("tool", "Edit(src/checkout/total.ts)", 1);
const GREP: BranchStep = ("tool", "Grep(applyDiscount)", 1);
const DELEGATE: BranchStep = ("tool", "Agent(test-investigator)", 1);
const INVESTIGATE: BranchStep = ("agent", "test-investigator", 1);
const HAND_BACK: BranchStep = ("tool", "mcp__github__add_issue_comment", 1);

/// List prices of the coding agent's models, USD per million tokens: input,
/// output, cache read and five-minute cache write, as Anthropic lists them
/// (2026-10-18). Haiku's higher rates for prompts over 100K tokens never
/// apply: its prompts here stay far below that.
const CODER_PRICES: &[(&str, [f64; 4])] = &[
    ("claude-sonnet-5-5", [2.0, 10.0, 0.10, 2.50]),
    ("claude-haiku-5-5", [0.10, 0.50, 0.01, 0.125]),
];

/// The branches, with their exact outcome counts. Support agents mostly
/// refund cleanly, sometimes escalate, and fail hard when they get stuck
/// re-running the same search; the researcher's fate depends on which
/// source it reaches for first. The coding agent fixes and verifies, hands
/// the investigation to a sub-agent on a smaller model, or hands the job
/// back to a maintainer — and fails hard when it re-runs the failing test
/// as though it were flaky, a newer push cancelling some of those runs.
const BRANCHES: &[Branch] = &[
    Branch {
        agent: "support-agent",
        name: "refund_happy",
        steps: &[
            GPT,
            ("tool", "search_orders", 1),
            GPT,
            ("tool", "issue_refund", 1),
            GPT,
        ],
        runs: 60,
        success: 54,
        abandoned: 0,
        goal: None,
    },
    Branch {
        agent: "support-agent",
        name: "search_loop",
        steps: &[GPT, ("tool", "search_orders", 4), GPT],
        runs: 50,
        success: 5,
        abandoned: 5,
        goal: None,
    },
    Branch {
        agent: "support-agent",
        name: "escalate",
        steps: &[
            GPT,
            ("tool", "lookup_customer", 1),
            GPT,
            ("tool", "escalate_ticket", 1),
            GPT,
        ],
        runs: 40,
        success: 24,
        abandoned: 4,
        goal: Some("escalate billing dispute"),
    },
    Branch {
        agent: "support-agent",
        name: "refund_retry",
        steps: &[
            GPT,
            ("tool", "search_orders", 1),
            GPT,
            ("tool", "issue_refund", 2),
            GPT,
        ],
        runs: 30,
        success: 15,
        abandoned: 0,
        goal: None,
    },
    Branch {
        agent: "researcher",
        name: "web_search",
        steps: &[SONNET, ("tool", "web_search", 1), SONNET],
        runs: 10,
        success: 9,
        abandoned: 0,
        goal: None,
    },
    Branch {
        agent: "researcher",
        name: "read_url",
        steps: &[SONNET, ("tool", "read_url", 1), SONNET],
        runs: 10,
        success: 7,
        abandoned: 0,
        goal: None,
    },
    Branch {
        agent: "researcher",
        name: "query_db",
        steps: &[SONNET, ("tool", "query_db", 1), SONNET],
        runs: 10,
        success: 3,
        abandoned: 0,
        goal: None,
    },
    Branch {
        agent: "researcher",
        name: "ask_human",
        steps: &[SONNET, ("tool", "ask_human", 1), SONNET],
        runs: 10,
        success: 1,
        abandoned: 0,
        goal: None,
    },
    Branch {
        agent: CODER,
        name: "fix_and_verify",
        steps: &[
            SONNET, TEST, SONNET, READ, SONNET, EDIT, SONNET, TEST, SONNET,
        ],
        runs: 50,
        success: 45,
        abandoned: 0,
        goal: None,
    },
    Branch {
        agent: CODER,
        name: "rerun_loop",
        steps: &[
            SONNET,
            ("tool", "Bash(npm test -- tests/e2e/checkout.spec.ts)", 4),
            SONNET,
        ],
        runs: 40,
        success: 3,
        abandoned: 7,
        goal: None,
    },
    Branch {
        agent: CODER,
        name: "delegate",
        steps: &[
            SONNET,
            DELEGATE,
            INVESTIGATE,
            HAIKU,
            GREP,
            HAIKU,
            SONNET,
            EDIT,
            SONNET,
            TEST,
            SONNET,
        ],
        runs: 45,
        success: 38,
        abandoned: 0,
        goal: None,
    },
    Branch {
        agent: CODER,
        name: "hand_back",
        steps: &[SONNET, TEST, SONNET, HAND_BACK, SONNET],
        runs: 30,
        success: 20,
        abandoned: 0,
        goal: None,
    },
];

/// The live demonstration runs: agent, id and steps so far. The support
/// agent's come last, so theirs are the latest spans to arrive and the
/// first six of a live view, which lists the most recently changed first.
const LIVE: &[(&str, &str, &[BranchStep])] = &[
    (
        CODER,
        "live-ci-doomed-0",
        &[
            SONNET,
            ("tool", "Bash(npm test -- tests/e2e/checkout.spec.ts)", 3),
        ],
    ),
    (
        CODER,
        "live-ci-healthy-0",
        &[SONNET, TEST, SONNET, READ, SONNET, EDIT],
    ),
    (
        CODER,
        "live-ci-novel-0",
        &[SONNET, TEST, SONNET, ("tool", "Bash(rm -rf tests/e2e)", 1)],
    ),
    (
        "support-agent",
        "live-doomed-0",
        &[GPT, ("tool", "search_orders", 4)],
    ),
    (
        "support-agent",
        "live-doomed-1",
        &[GPT, ("tool", "search_orders", 3)],
    ),
    (
        "support-agent",
        "live-doomed-2",
        &[GPT, ("tool", "search_orders", 5)],
    ),
    (
        "support-agent",
        "live-healthy-0",
        &[
            GPT,
            ("tool", "search_orders", 1),
            GPT,
            ("tool", "issue_refund", 1),
        ],
    ),
    (
        "support-agent",
        "live-novel-0",
        &[GPT, ("tool", "delete_database", 1)],
    ),
    ("support-agent", "live-early-0", &[GPT]),
];

/// Whether item `index` of `total` is among `chosen` spread evenly across
/// them (Bresenham), so any newest-first prefix of a branch stays close to
/// its proportions.
fn spread(index: usize, total: usize, chosen: usize) -> bool {
    total > 0 && (index + 1) * chosen / total > index * chosen / total
}

/// A corpus of agent runs whose behaviour branches have known, different
/// success rates — the shape the Fate Map ([`crate::fate`]) learns from.
///
/// Three cohorts. `support-agent` runs (service `support-agent`, under a
/// workflow root that is not a step) take one of four branches: a clean
/// refund (54 of 60 succeed), a search re-run four times in a row (5 of 50
/// succeed and 5 are abandoned — the fatal fork), an escalation (24 of 40,
/// 4 abandoned, each declaring the goal `escalate billing dispute`), and a
/// refund issued twice (15 of 30). `researcher` runs (an `invoke_agent`
/// root naming `gen_ai.agent.name: researcher`, service `research-svc`)
/// pick one of four sources, with 9, 7, 3 and 1 successes in 10 — a
/// hotspot. `ci-fix` runs are Claude Code in a CI job (service `ci-fix`,
/// under a `claude_code.interaction` root that is not a step), with Claude
/// Code's tool names and its usage — a prompt read mostly from the cache,
/// metered at the models' list prices. After the failing test's first run
/// they read the code, fix it and run the test again (45 of 50), or hand the
/// job back with a PR comment (20 of 30: 23 points worse than fixing, short
/// of a fatal fork); they delegate to a `test-investigator` sub-agent on
/// `claude-haiku-5-5` first (38 of 45); or they re-run the failing test four
/// times in a row (3 of 40, 7 abandoned — the fatal fork). Failed runs end
/// on an error status; abandoned runs declare `session.outcome: abandoned`.
///
/// Deterministic: the same options give the same spans, byte for byte, and
/// every outcome count is exact (times `scale`).
pub fn behaviour_corpus(options: &BehaviourOptions) -> BehaviourCorpus {
    let mut rng = Rng::new(options.seed);
    let scale = options.scale.max(1);
    let mut out = BehaviourCorpus::default();
    let namespace = options
        .prefix
        .bytes()
        .chain(options.tenant.bytes())
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        });
    let mut serial = 0_u64;
    let completed_end = options.now_ns.saturating_sub(2 * 3_600 * SEC);
    for branch in BRANCHES {
        let runs = branch.runs * scale;
        let successes = branch.success * scale;
        let mut others = 0;
        for index in 0..runs {
            let outcome = if spread(index, runs, successes) {
                "success"
            } else {
                others += 1;
                if spread(others - 1, runs - successes, branch.abandoned * scale) {
                    "abandoned"
                } else {
                    "failure"
                }
            };
            let start = completed_end.saturating_sub(serial * 45 * SEC);
            let run = format!("{}{}-{index}", options.prefix, branch.name);
            let plan = RunPlan {
                run: &run,
                agent: branch.agent,
                steps: branch.steps,
                start,
                outcome,
                goal: branch.goal,
            };
            push_behaviour_run(&mut out, &mut rng, options, namespace, serial, &plan);
            out.runs.push(BehaviourRun {
                run,
                agent: branch.agent,
                branch: branch.name,
                outcome,
            });
            serial += 1;
        }
    }
    if options.live {
        for (offset, &(agent, name, steps)) in LIVE.iter().enumerate() {
            let run = format!("{}{name}", options.prefix);
            let plan = RunPlan {
                run: &run,
                agent,
                steps,
                start: options
                    .now_ns
                    .saturating_sub(40 * SEC + offset as u64 * SEC),
                outcome: "live",
                goal: None,
            };
            push_behaviour_run(&mut out, &mut rng, options, namespace, serial, &plan);
            out.runs.push(BehaviourRun {
                run,
                agent,
                branch: "live",
                outcome: "live",
            });
            serial += 1;
        }
    }
    out
}

/// One run to generate.
struct RunPlan<'a> {
    run: &'a str,
    agent: &'static str,
    steps: &'a [BranchStep],
    start: u64,
    outcome: &'static str,
    goal: Option<&'static str>,
}

/// Appends one run's spans: its steps in order, then its root — which a
/// live run has not ended yet, and so has not exported. An `agent` step
/// starts a sub-agent under the tool call before it, as Claude Code's
/// `Agent` tool does: its own span, holding the steps that follow until the
/// run's first model takes its turn again, while the call waits on it.
fn push_behaviour_run(
    out: &mut BehaviourCorpus,
    rng: &mut Rng,
    options: &BehaviourOptions,
    namespace: u64,
    serial: u64,
    plan: &RunPlan<'_>,
) {
    let trace = format!("{namespace:016x}{serial:016x}");
    let root_id = format!("{:016x}", serial << 8);
    let researcher = plan.agent == "researcher";
    let coding = plan.agent == CODER;
    let service = if researcher {
        "research-svc"
    } else {
        plan.agent
    };
    let live = plan.outcome == "live";
    let looping = plan
        .steps
        .iter()
        .any(|(_, name, repeats)| *name == "search_orders" && *repeats > 1);
    let mut clock = plan.start + 5 * MS;
    let flat: Vec<(&str, &str)> = plan
        .steps
        .iter()
        .flat_map(|(kind, name, repeats)| std::iter::repeat((*kind, *name)).take(*repeats))
        .collect();
    let total = flat.len();
    let main_model = flat.first().map(|(_, name)| *name);
    let mut steps: Vec<Span> = Vec::with_capacity(total);
    // The sub-agent at work: its name, and the indexes of the call that
    // started it and of its own span.
    let mut delegated: Option<(&str, usize, usize)> = None;
    // Model calls so far: the run's own, then its sub-agent's.
    let mut turns = [0_u64; 2];
    let mut edited = false;
    for (position, &(kind, step)) in flat.iter().enumerate() {
        if kind == "llm" && Some(step) == main_model {
            if let Some((_, call, agent)) = delegated.take() {
                end_delegation(&mut steps, call, agent);
            }
        }
        let (name, target) = split_call(step);
        let duration = if coding {
            coding_duration(rng, kind, name)
        } else {
            rng.range(300, 1_200) * MS
        };
        let last = position + 1 == total;
        let mut attributes = Map::new();
        attributes.insert("session.id".into(), json!(plan.run));
        let parent = match (delegated, kind) {
            (Some((agent, _, span)), _) => {
                attributes.insert("gen_ai.agent.name".into(), json!(agent));
                steps[span].span_id.clone()
            }
            (None, "agent") => steps
                .last()
                .map_or_else(|| root_id.clone(), |call| call.span_id.clone()),
            (None, _) => root_id.clone(),
        };
        let (span_name, status) = match kind {
            "llm" => {
                attributes.insert("gen_ai.operation.name".into(), json!("chat"));
                attributes.insert("gen_ai.request.model".into(), json!(name));
                if coding {
                    let turn = &mut turns[usize::from(delegated.is_some())];
                    coding_usage(rng, &mut attributes, name, *turn, delegated.is_some());
                    *turn += 1;
                } else {
                    attributes.insert(
                        "gen_ai.usage.input_tokens".into(),
                        json!(600 + 40 * position as u64),
                    );
                    attributes.insert("gen_ai.usage.output_tokens".into(), json!(90));
                    attributes.insert("llm.cost_usd".into(), json!(0.002));
                }
                let failed = last && plan.outcome == "failure";
                (format!("chat {name}"), if failed { "error" } else { "ok" })
            }
            "agent" => {
                attributes.insert("gen_ai.operation.name".into(), json!("invoke_agent"));
                attributes.insert("gen_ai.agent.name".into(), json!(name));
                (format!("invoke_agent {name}"), "ok")
            }
            _ => {
                attributes.insert("gen_ai.operation.name".into(), json!("execute_tool"));
                attributes.insert("gen_ai.tool.name".into(), json!(name));
                let failed = if coding {
                    let (arguments, result, failed) = coding_call(name, target, edited);
                    attributes.insert(
                        "gen_ai.tool.call.arguments".into(),
                        json!(arguments.to_string()),
                    );
                    if !result.is_empty() {
                        attributes.insert("gen_ai.tool.call.result".into(), json!(result));
                    }
                    edited |= name == "Edit";
                    failed
                } else {
                    attributes.insert(
                        "gen_ai.tool.call.arguments".into(),
                        json!(format!("{{\"attempt\": {position}}}")),
                    );
                    // A search re-run in a row is a search that came back empty.
                    looping && name == "search_orders" && !last
                };
                (
                    format!("execute_tool {name}"),
                    if failed { "error" } else { "ok" },
                )
            }
        };
        let mut span = make_span(
            &trace,
            &format!("{:016x}", (serial << 8) + position as u64 + 1),
            Some(&parent),
            &span_name,
            service,
            clock,
            duration,
            status,
            attributes,
        );
        span.tenant = options.tenant.clone();
        if kind == "agent" {
            delegated = Some((name, steps.len().saturating_sub(1), steps.len()));
        }
        steps.push(span);
        // A call that starts a sub-agent, and the sub-agent's span, hold the
        // steps that follow: the next one starts inside them.
        let opens = flat
            .get(position + 1)
            .is_some_and(|(next, _)| *next == "agent");
        clock += if opens || kind == "agent" {
            5 * MS
        } else {
            duration + 50 * MS
        };
    }
    if let Some((_, call, agent)) = delegated {
        end_delegation(&mut steps, call, agent);
    }
    if !live {
        let mut attributes = Map::new();
        attributes.insert("session.id".into(), json!(plan.run));
        let root_name = if researcher {
            attributes.insert("gen_ai.operation.name".into(), json!("invoke_agent"));
            attributes.insert("gen_ai.agent.name".into(), json!("researcher"));
            "invoke_agent researcher"
        } else if coding {
            // The Claude Code release the runs report.
            attributes.insert("service.version".into(), json!("2.1.303"));
            "claude_code.interaction"
        } else {
            attributes.insert("traceloop.span.kind".into(), json!("workflow"));
            attributes.insert("traceloop.entity.name".into(), json!("handle_ticket"));
            "support.handle_ticket"
        };
        if let Some(goal) = plan.goal {
            attributes.insert("session.goal".into(), json!(goal));
        }
        if plan.outcome == "abandoned" {
            attributes.insert("session.outcome".into(), json!("abandoned"));
        }
        let mut root = make_span(
            &trace,
            &root_id,
            None,
            root_name,
            service,
            plan.start,
            clock + 10 * MS - plan.start,
            if plan.outcome == "failure" {
                "error"
            } else {
                "ok"
            },
            attributes,
        );
        root.tenant = options.tenant.clone();
        out.spans.push(root);
    }
    out.spans.extend(steps);
}

/// Ends a delegation: the sub-agent's span closes just after its last step,
/// and the call that waited on it just after that.
fn end_delegation(steps: &mut [Span], call: usize, agent: usize) {
    let done = steps[agent + 1..]
        .iter()
        .map(|step| step.end_time_ns)
        .max()
        .unwrap_or(steps[agent].end_time_ns);
    steps[agent].end_time_ns = done + MS;
    steps[call].end_time_ns = steps[call].end_time_ns.max(done + 2 * MS);
}

/// A step's tool and what it was called on: `Bash(npm test)` is `Bash` on
/// `npm test`; a name without parentheses names the tool alone.
fn split_call(step: &str) -> (&str, &str) {
    match step.split_once('(') {
        Some((tool, rest)) => (tool, rest.strip_suffix(')').unwrap_or(rest)),
        None => (step, ""),
    }
}

/// How long one of the coding agent's steps takes: seconds for a model call
/// (fewer on Haiku) and for a test run, a moment for anything else.
fn coding_duration(rng: &mut Rng, kind: &str, name: &str) -> u64 {
    match (kind, name) {
        ("llm", "claude-haiku-5-5") => rng.range(600, 1_800) * MS,
        ("llm", _) => rng.range(1_500, 4_000) * MS,
        (_, "Bash") => rng.range(2_000, 5_000) * MS,
        _ => rng.range(40, 400) * MS,
    }
}

/// One of the coding agent's model calls: its usage, and its cost metered
/// at the model's list price. Claude Code marks its prompt for caching, so
/// nearly all of it is read from the cache — the system prompt, the tools
/// and the conversation so far, which grows every turn — and the turn's new
/// content is written to it. A sub-agent starts a smaller context.
fn coding_usage(
    rng: &mut Rng,
    attributes: &mut Map<String, Value>,
    model: &str,
    turn: u64,
    sub_agent: bool,
) {
    let (context, growth, output) = if sub_agent {
        (7_500, 1_600, rng.range(80, 900))
    } else {
        (17_000, 2_400, rng.range(150, 2_400))
    };
    let input = rng.range(4, 60);
    let cache_read = context + growth * turn + rng.range(0, 800);
    let cache_write = rng.range(400, 3_000);
    let [per_input, per_output, per_read, per_write] = CODER_PRICES
        .iter()
        .find(|(name, _)| *name == model)
        .map_or([0.0; 4], |(_, price)| *price);
    let cost = (input as f64 * per_input
        + output as f64 * per_output
        + cache_read as f64 * per_read
        + cache_write as f64 * per_write)
        / 1_000_000.0;
    attributes.insert("gen_ai.provider.name".into(), json!("anthropic"));
    attributes.insert("gen_ai.usage.input_tokens".into(), json!(input));
    attributes.insert("gen_ai.usage.output_tokens".into(), json!(output));
    attributes.insert(
        "gen_ai.usage.cache_read.input_tokens".into(),
        json!(cache_read),
    );
    attributes.insert(
        "gen_ai.usage.cache_creation.input_tokens".into(),
        json!(cache_write),
    );
    attributes.insert("llm.cost_usd".into(), json!(round_cents(cost)));
}

/// What one of the coding agent's tool calls was given, what it returned,
/// and whether it failed: the job's test fails until the run has edited the
/// code, and every other call succeeds.
fn coding_call(tool: &str, target: &str, edited: bool) -> (Value, &'static str, bool) {
    let file = |path: &str| format!("/home/runner/work/shop/shop/{path}");
    match tool {
        "Bash" if target.starts_with("npm test") => (
            json!({"command": target, "description": "Run the failing checkout test"}),
            if edited {
                "Test Files  1 passed (1)\n     Tests  6 passed (6)"
            } else {
                "FAIL tests/e2e/checkout.spec.ts > a full discount makes the order free\nAssertionError: expected -12.5 to be 0\n\nTest Files  1 failed (1)\n     Tests  1 failed | 5 passed (6)"
            },
            !edited,
        ),
        "Bash" => (json!({"command": target}), "", false),
        "Read" => (
            json!({"file_path": file(target)}),
            "1\timport { applyDiscount } from \"./discounts\";\n2\t\n3\texport function total(cart: Cart, code: string): number {\n4\t  const discount = applyDiscount(cart, code);\n5\t  return cart.subtotal - discount;\n6\t}",
            false,
        ),
        "Edit" => (
            json!({"file_path": file(target),
                   "old_string": "return cart.subtotal - discount;",
                   "new_string": "return Math.max(cart.subtotal - discount, 0);"}),
            "The file has been updated.",
            false,
        ),
        "Grep" => (
            json!({"pattern": target, "path": "src", "output_mode": "content"}),
            "src/checkout/discounts.ts:12:export function applyDiscount(cart: Cart, code: string): number {\nsrc/checkout/total.ts:1:import { applyDiscount } from \"./discounts\";\nsrc/checkout/total.ts:4:  const discount = applyDiscount(cart, code);",
            false,
        ),
        "Agent" => (
            json!({"subagent_type": target,
                   "description": "Find why the checkout test fails",
                   "prompt": "tests/e2e/checkout.spec.ts fails in CI. Find the cause and report it; do not edit files."}),
            "total() subtracts the discount with no floor, so a discount larger than the subtotal makes the total negative (src/checkout/total.ts:5).",
            false,
        ),
        "mcp__github__add_issue_comment" => (
            json!({"owner": "acme", "repo": "shop", "issue_number": 4182,
                   "body": "The checkout e2e test still fails: total() returns -12.5 where the test expects 0 for a full discount. Fixing it means deciding how a discount above the subtotal is priced, which is a product call, so I am handing it back. No code was changed."}),
            "{\"id\":2417730912,\"html_url\":\"https://github.com/acme/shop/pull/4182#issuecomment-2417730912\"}",
            false,
        ),
        _ => (json!({}), "", false),
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
    fn claude_calls_send_no_temperature_and_cached_tokens_are_metered() {
        let generated = corpus(&SeedOptions::default());
        let model = |span: &Span| {
            ["gen_ai.request.model", "llm.model"]
                .iter()
                .find_map(|key| span.attributes.get(*key)?.as_str())
                .unwrap_or_default()
                .to_owned()
        };
        let (mut claude, mut tempered, mut cached) = (0, 0, 0);
        for span in &generated.spans {
            let tempered_here = span.attributes.contains_key("gen_ai.request.temperature");
            if model(span).contains("claude") {
                assert!(!tempered_here, "{} sent a temperature", model(span));
                claude += 1;
            }
            tempered += usize::from(tempered_here);
            let Some(read) = span
                .attributes
                .get("gen_ai.usage.cache_read.input_tokens")
                .and_then(Value::as_u64)
            else {
                continue;
            };
            let count = |key: &str| span.attributes[key].as_u64().expect(key) as f64;
            // claude-sonnet-5-5's list price per 1K tokens, the cache apart.
            let expected = (count("gen_ai.usage.input_tokens") * 0.002
                + count("gen_ai.usage.output_tokens") * 0.010
                + read as f64 * 0.0001
                + count("gen_ai.usage.cache_creation.input_tokens") * 0.0025)
                / 1000.0;
            let metered = span.attributes["llm.cost_usd"].as_f64().expect("metered");
            assert!((metered - expected).abs() < 1e-6, "{metered} vs {expected}");
            cached += 1;
        }
        assert!(claude > 0 && tempered > 0 && cached > 0);
    }

    #[test]
    fn the_behaviour_corpus_is_exact_and_deterministic() {
        let options = BehaviourOptions {
            live: true,
            ..BehaviourOptions::default()
        };
        let first = behaviour_corpus(&options);
        let second = behaviour_corpus(&options);
        assert_eq!(first.spans, second.spans);
        assert_eq!(first.runs, second.runs);
        let mut keys = HashSet::new();
        for span in &first.spans {
            assert!(
                keys.insert((span.trace_id.clone(), span.span_id.clone())),
                "duplicate span identity: {}/{}",
                span.trace_id,
                span.span_id
            );
        }
        let count = |branch: &str, outcome: &str| {
            first
                .runs
                .iter()
                .filter(|run| run.branch == branch && run.outcome == outcome)
                .count()
        };
        assert_eq!(
            (
                count("refund_happy", "success"),
                count("refund_happy", "failure")
            ),
            (54, 6)
        );
        assert_eq!(
            (
                count("search_loop", "success"),
                count("search_loop", "abandoned"),
                count("search_loop", "failure")
            ),
            (5, 5, 40)
        );
        assert_eq!(
            (
                count("escalate", "success"),
                count("escalate", "abandoned"),
                count("escalate", "failure")
            ),
            (24, 4, 12)
        );
        assert_eq!(
            (
                count("fix_and_verify", "success"),
                count("rerun_loop", "success"),
                count("rerun_loop", "abandoned"),
                count("rerun_loop", "failure"),
                count("delegate", "success"),
                count("hand_back", "success")
            ),
            (45, 3, 7, 30, 38, 20)
        );
        assert_eq!(count("live", "live"), 9);
        // Completed runs end two hours before "now"; live ones within its
        // last minute.
        let live = |span: &&Span| {
            span.attributes["session.id"]
                .as_str()
                .unwrap_or_default()
                .starts_with("live-")
        };
        let latest_completed = first
            .spans
            .iter()
            .filter(|span| !live(span))
            .map(|span| span.end_time_ns)
            .max()
            .unwrap_or_default();
        assert!(latest_completed < options.now_ns - 3_600 * SEC);
        assert!(first
            .spans
            .iter()
            .filter(live)
            .all(|span| span.end_time_ns <= options.now_ns
                && span.end_time_ns > options.now_ns - 60 * SEC));
        let scaled = behaviour_corpus(&BehaviourOptions {
            scale: 2,
            ..BehaviourOptions::default()
        });
        assert_eq!(scaled.runs.len(), 2 * 385);
        let tenanted = behaviour_corpus(&BehaviourOptions {
            tenant: "acme".to_owned(),
            ..BehaviourOptions::default()
        });
        assert!(tenanted.spans.iter().all(|span| span.tenant == "acme"));
        assert_ne!(tenanted.spans[0].trace_id, first.spans[0].trace_id);
    }

    #[test]
    fn the_coding_agent_meters_its_calls_and_nests_its_sub_agent() {
        let generated = behaviour_corpus(&BehaviourOptions::default());
        // Every model call is metered at its model's list price, the cache
        // reads and writes included, over a prompt of thousands of tokens.
        let mut calls = 0;
        for span in generated
            .spans
            .iter()
            .filter(|span| span.service == "ci-fix" && span.name.starts_with("chat "))
        {
            let tokens = |key: &str| span.attributes[key].as_u64().expect(key) as f64;
            let [input, output, read, write] = match span.attributes["gen_ai.request.model"]
                .as_str()
                .unwrap_or_default()
            {
                "claude-sonnet-5-5" => [2.0, 10.0, 0.10, 2.50],
                "claude-haiku-5-5" => [0.10, 0.50, 0.01, 0.125],
                other => panic!("an unpriced model {other}"),
            };
            let expected = (tokens("gen_ai.usage.input_tokens") * input
                + tokens("gen_ai.usage.output_tokens") * output
                + tokens("gen_ai.usage.cache_read.input_tokens") * read
                + tokens("gen_ai.usage.cache_creation.input_tokens") * write)
                / 1_000_000.0;
            let metered = span.attributes["llm.cost_usd"].as_f64().expect("metered");
            assert!((metered - expected).abs() < 1e-6, "{metered} vs {expected}");
            assert!(tokens("gen_ai.usage.cache_read.input_tokens") >= 7_500.0);
            assert_eq!(span.attributes["gen_ai.provider.name"], "anthropic");
            calls += 1;
        }
        assert!(calls > 0);
        let run = |id: &str| -> Vec<&Span> {
            generated
                .spans
                .iter()
                .filter(|span| span.attributes["session.id"] == id)
                .collect()
        };
        // The test fails until the code is edited, and a loop never edits.
        let tests = |id: &str| -> Vec<String> {
            run(id)
                .into_iter()
                .filter(|span| span.attributes.get("gen_ai.tool.name") == Some(&json!("Bash")))
                .map(|span| span.status.clone())
                .collect()
        };
        assert_eq!(tests("fix_and_verify-0"), ["error", "ok"]);
        assert_eq!(tests("rerun_loop-0"), ["error"; 4]);
        // The sub-agent's span hangs off the Agent call, and holds its own
        // steps — on Haiku — inside the time the call waited.
        let delegated = run("delegate-0");
        let find = |name: &str| {
            *delegated
                .iter()
                .find(|span| span.name == name)
                .unwrap_or_else(|| panic!("no {name}"))
        };
        let call = find("execute_tool Agent");
        let agent = find("invoke_agent test-investigator");
        assert_eq!(agent.parent_span_id.as_deref(), Some(call.span_id.as_str()));
        let inner: Vec<&Span> = delegated
            .iter()
            .copied()
            .filter(|span| span.parent_span_id.as_deref() == Some(agent.span_id.as_str()))
            .collect();
        let names: Vec<&str> = inner.iter().map(|span| span.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "chat claude-haiku-5-5",
                "execute_tool Grep",
                "chat claude-haiku-5-5"
            ]
        );
        for span in &inner {
            assert_eq!(span.attributes["gen_ai.agent.name"], "test-investigator");
            assert!(span.start_time_ns > agent.start_time_ns);
            assert!(span.end_time_ns < agent.end_time_ns);
        }
        assert!(agent.end_time_ns < call.end_time_ns);
        let next = find("execute_tool Edit");
        assert!(next.start_time_ns > call.end_time_ns);
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
