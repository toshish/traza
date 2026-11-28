// The research run the README and the engine's replay and fork guides print,
// against a real traza-server.
//
// `dr-0917` is built span for span as the site's demo script records it: a
// coordinator on claude-opus-5-5 hands a web search to a web-researcher
// sub-agent on claude-haiku-5-5 through its `Agent` tool, then reads the
// sources with `WebFetch`; the sub-agent searches with `WebSearch`. The
// harness runs every tool itself, so each is a step. Prompts are OpenLLMetry's
// indexed layout, with the cache counters Anthropic reports. The README's
// replay loop runs here as written, with a few lines of fetch standing in for
// the Anthropic SDK's `messages.create` (the package depends on nothing), and
// the outputs the documents show are asserted exactly: the served brief, the
// diff against the deduping fork, recall, and the tape test's failure. The
// tests run the compiled package (dist/), exactly what an application imports.

import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, before, describe, it } from "node:test";

import { Client, TapeMismatch, replayTest } from "../dist/index.js";
import { startServer } from "./server.js";

// 2026-01-01T00:00:00Z: old enough that both runs read as finished.
const T0 = 1_767_225_600_000_000_000n;
const MS = 1_000_000n;

const COORDINATOR =
  "You are a research coordinator. Delegate web searches to the web-researcher subagent, read the sources it finds with WebFetch, then write a one-page brief that cites every source by URL.";
const WORKER =
  "You are web-researcher. Search the web and reply with the most relevant primary sources, one line each, with their URLs.";
const TASK = "Write a one-page brief on solid-state battery readiness for EVs in 2027, with sources.";
const BRIEF = "Find 2026 primary sources on solid-state battery pilot production, yields and cost per kWh.";
const QUERY = "solid-state battery pilot line yield cost per kWh 2026";
const FETCH_PROMPT = "List the 2026 figures on pilot lines, yields, cost per kWh and sample cells.";
const URLS = [
  "https://www.example-energy.org/2026/solid-state-pilot-lines",
  "https://www.example-batteries.com/reports/ssb-cost-2026",
  "https://www.example-auto.net/news/2026-ssb-sample-cells",
];
const PAGES = [
  "Pilot lines (2026): three cell makers ran solid-state pilot lines at 0.2 to 1 GWh a year. First-pass yields were reported between 60% and 75%.",
  "Cost (2026): solid-state cells were estimated at $180 to $240 per kWh, against about $95 for LFP. The gap narrows only with sulfide electrolyte scale-up after 2028.",
  "Samples (2026): two automakers began road tests of B-sample solid-state cells late in the year. Neither committed to a 2027 production vehicle.",
];
const PAGE_TOKENS = [6_200, 5_400, 4_800];
const SEARCH = URLS.map((url, index) => `${index + 1}. ${url}`).join("\n");
const FINDINGS = `Sources:\n${URLS.map((url, index) => `- ${url}: ${PAGES[index].split(":")[0]}`).join("\n")}`;
const FINAL =
  "Solid-state batteries in 2027: pilot scale, not production. Pilot lines ran at 0.2 to 1 GWh a year in 2026 with 60 to 75% first-pass yields; cells cost $180 to $240 per kWh against about $95 for LFP; automakers are road-testing B-samples but none has committed to a 2027 vehicle. Expect limited premium launches after 2028.";
const AGENTS = {
  coordinator: { model: "claude-opus-5-5", agent: "research-coordinator", system: COORDINATOR, prefix: 9_800 },
  worker: { model: "claude-haiku-5-5", agent: "web-researcher", system: WORKER, prefix: 2_600 },
};
// The list rates the guides' server was started with (`--pricing`).
const PRICING = {
  models: {
    "claude-opus-5-5": { input_per_mtok: 4.0, output_per_mtok: 20.0, cache_read_per_mtok: 0.2, cache_write_per_mtok: 5.0 },
    "claude-haiku-5-5": { input_per_mtok: 0.1, output_per_mtok: 0.5, cache_read_per_mtok: 0.01, cache_write_per_mtok: 0.125 },
  },
};

const sha = (text) => createHash("sha256").update(text).digest("hex");
const dedupeNote = (url) => `Already fetched ${url} earlier in this run; reusing that result.`;

/** The run as the harness lives it: model calls (`agent`) and tool calls (`run`). */
function plan(fetches, { dedupe = false } = {}) {
  const steps = [
    { agent: "coordinator", tool: "Agent", input: { description: "Battery sources", prompt: BRIEF, subagent_type: "web-researcher" } },
    { agent: "worker", tool: "WebSearch", input: { query: QUERY } },
    { run: "WebSearch", result: SEARCH, tokens: 380 },
    { agent: "worker", text: FINDINGS, final: true },
  ];
  const seen = new Set();
  for (const page of fetches) {
    steps.push({ agent: "coordinator", tool: "WebFetch", input: { url: URLS[page], prompt: FETCH_PROMPT } });
    steps.push(
      dedupe && seen.has(page)
        ? { run: "WebFetch", result: dedupeNote(URLS[page]), tokens: 40, deduped: true }
        : { run: "WebFetch", result: PAGES[page], tokens: PAGE_TOKENS[page] },
    );
    seen.add(page);
  }
  steps.push({ agent: "coordinator", text: FINAL });
  return steps;
}

/** Each agent's conversation as the harness sends it, with the usage the provider reports. */
function callsOf(steps, key) {
  const convo = {
    coordinator: { messages: [{ role: "user", content: TASK }], ctx: 40, cached: -1 },
    worker: { messages: [{ role: "user", content: BRIEF }], ctx: 30, cached: -1 },
  };
  const calls = [];
  let pending = null;
  let taskId = null;
  let asked = 0;
  for (const step of steps) {
    if (step.agent) {
      const c = convo[step.agent];
      const { prefix } = AGENTS[step.agent];
      const read = c.cached < 0 ? 0 : prefix + c.cached;
      const write = c.cached < 0 ? prefix + c.ctx : c.ctx - c.cached;
      const sent = structuredClone(c.messages);
      let content;
      let output;
      if (step.tool) {
        const id = `toolu_${sha(`${key}:${++asked}`).slice(0, 22)}`;
        content = [{ type: "tool_use", id, name: step.tool, input: step.input }];
        pending = { agent: step.agent, id };
        if (step.tool === "Agent") taskId = id;
        output = 60 + Math.ceil(JSON.stringify(step.input).length / 4);
      } else {
        content = [{ type: "text", text: step.text }];
        output = step.agent === "coordinator" ? 1_900 : 720;
      }
      c.cached = c.ctx;
      c.ctx += output;
      c.messages.push({ role: "assistant", content });
      calls.push({ ...step, sent, content, usage: [3 + (asked % 5), output, read, write] });
      if (step.final) {
        convo.coordinator.messages.push({ role: "user", content: [{ type: "tool_result", tool_use_id: taskId, content: step.text }] });
        convo.coordinator.ctx += output;
        calls.push({ closes: true, result: step.text });
      }
    } else {
      const c = convo[pending.agent];
      c.messages.push({ role: "user", content: [{ type: "tool_result", tool_use_id: pending.id, content: step.result }] });
      c.ctx += step.tokens;
      calls.push({ ...step, agent: pending.agent, id: pending.id });
    }
  }
  return calls;
}

/** The run's spans in Traza's native JSON; `extra` lands on every span (a fork's tags). */
function spansOf({ run, key, start, calls, extra = {} }) {
  const trace = sha(`trace:${key}`).slice(0, 32);
  let serial = 0;
  const nextId = () => sha(`span:${key}:${++serial}`).slice(0, 16);
  const attrs = (more) => ({ "session.id": run, ...extra, ...more });
  const span = (spanId, parent, name, begin, ms, attributes) => ({
    trace_id: trace,
    span_id: spanId,
    parent_span_id: parent,
    name,
    service: "research-coordinator",
    status: "ok",
    start_time_unix_nano: begin,
    end_time_unix_nano: begin + BigInt(ms) * MS,
    attributes,
  });
  const inputs = new Map(calls.filter((c) => c.content && c.tool).map((c) => [c.content[0].id, c.input]));
  const root = nextId();
  const spans = [];
  let clock = start + 5n * MS;
  let task = null;
  let sub = null;
  for (const c of calls) {
    if (c.closes) {
      sub.end_time_unix_nano = clock;
      task.end_time_unix_nano = clock + 2n * MS;
      task.attributes["gen_ai.tool.call.result"] = c.result;
      clock += 10n * MS;
    } else if (c.content) {
      const who = AGENTS[c.agent];
      const ms = c.agent === "coordinator" ? (c.text ? 21_400 : 3_600) : c.text ? 4_100 : 1_200;
      const a = attrs({
        "gen_ai.operation.name": "chat",
        "gen_ai.system": "anthropic",
        "gen_ai.provider.name": "anthropic",
        "gen_ai.request.model": who.model,
        "gen_ai.response.model": who.model,
        "gen_ai.agent.name": who.agent,
        "gen_ai.usage.input_tokens": c.usage[0],
        "gen_ai.usage.output_tokens": c.usage[1],
        "gen_ai.usage.cache_read.input_tokens": c.usage[2],
        "gen_ai.usage.cache_creation.input_tokens": c.usage[3],
        "gen_ai.prompt.0.role": "system",
        "gen_ai.prompt.0.content": who.system,
      });
      c.sent.forEach((message, index) => {
        a[`gen_ai.prompt.${index + 1}.role`] = message.role;
        a[`gen_ai.prompt.${index + 1}.content`] = typeof message.content === "string" ? message.content : JSON.stringify(message.content);
      });
      a["gen_ai.completion.0.role"] = "assistant";
      if (c.text) {
        a["gen_ai.completion.0.content"] = c.text;
        a["gen_ai.completion.0.finish_reason"] = "end_turn";
      } else {
        const use = c.content[0];
        a["gen_ai.completion.0.finish_reason"] = "tool_use";
        a["gen_ai.completion.0.tool_calls.0.id"] = use.id;
        a["gen_ai.completion.0.tool_calls.0.name"] = use.name;
        a["gen_ai.completion.0.tool_calls.0.arguments"] = JSON.stringify(use.input);
      }
      spans.push(span(nextId(), c.agent === "worker" ? sub.span_id : root, `chat ${who.model}`, clock, ms, a));
      clock += BigInt(ms + 40) * MS;
      if (c.tool === "Agent") {
        const use = c.content[0];
        task = span(nextId(), root, "execute_tool Agent", clock, 0, attrs({
          "gen_ai.operation.name": "execute_tool",
          "gen_ai.tool.name": "Agent",
          "gen_ai.agent.name": "research-coordinator",
          "gen_ai.tool.call.id": use.id,
          "gen_ai.tool.call.arguments": JSON.stringify(use.input),
        }));
        spans.push(task);
        clock += 5n * MS;
        sub = span(nextId(), task.span_id, "invoke_agent web-researcher", clock, 0, attrs({
          "gen_ai.operation.name": "invoke_agent",
          "gen_ai.agent.name": "web-researcher",
        }));
        spans.push(sub);
        clock += 5n * MS;
      }
    } else {
      const ms = c.run === "WebSearch" ? 900 : c.deduped ? 2 : 1_700;
      spans.push(span(nextId(), c.agent === "worker" ? sub.span_id : root, `execute_tool ${c.run}`, clock, ms, attrs({
        "gen_ai.operation.name": "execute_tool",
        "gen_ai.tool.name": c.run,
        "gen_ai.agent.name": AGENTS[c.agent].agent,
        "gen_ai.tool.call.id": c.id,
        "gen_ai.tool.call.arguments": JSON.stringify(inputs.get(c.id)),
        "gen_ai.tool.call.result": c.result,
      })));
      clock += BigInt(ms + 40) * MS;
    }
  }
  spans.unshift(span(root, null, "invoke_agent research-coordinator", start, Number((clock + 10n * MS - start) / MS), attrs({
    "gen_ai.operation.name": "invoke_agent",
    "gen_ai.agent.name": "research-coordinator",
  })));
  return spans;
}

/** What the Anthropic SDK's `messages.create` does with `baseURL` and `apiKey`: the README's loop runs against it unchanged. */
class Anthropic {
  constructor({ baseURL, apiKey }) {
    this.messages = {
      async create(body) {
        const answer = await fetch(`${baseURL}/v1/messages`, {
          method: "POST",
          headers: { "content-type": "application/json", "x-api-key": apiKey, "anthropic-version": "2023-06-01" },
          body: JSON.stringify(body),
        });
        const parsed = await answer.json();
        if (answer.status !== 200) throw Object.assign(new Error(`${answer.status} ${parsed.error?.type}`), { status: answer.status });
        return parsed;
      },
    };
  }
}

const tool = (name, field) => ({ name, input_schema: { type: "object", properties: { [field]: { type: "string" } } } });

/** The README's loop; `dedupe` is the harness that refuses to fetch a URL twice. */
async function research(anthropic, toolResult, { dedupe = false } = {}) {
  const fetched = new Set();
  async function run(model, system, task, tools) {
    const messages = [{ role: "user", content: task }];
    for (;;) {
      const answer = await anthropic.messages.create({ model, max_tokens: 8192, system, messages, tools });
      messages.push({ role: "assistant", content: answer.content });
      const calls = answer.content.filter((block) => block.type === "tool_use");
      if (!calls.length) return answer.content.map((block) => (block.type === "text" ? block.text : "")).join("");
      const results = [];
      for (const call of calls) {
        let content;
        if (call.name === "Agent") {
          content = await run("claude-haiku-5-5", WORKER, call.input.prompt, [tool("WebSearch", "query")]);
        } else if (dedupe && call.name === "WebFetch" && fetched.has(call.input.url)) {
          content = dedupeNote(call.input.url);
        } else {
          fetched.add(call.input.url);
          content = String((await toolResult(call.name))?.result);
        }
        results.push({ type: "tool_result", tool_use_id: call.id, content });
      }
      messages.push({ role: "user", content: results });
    }
  }
  return run("claude-opus-5-5", COORDINATOR, TASK, [tool("Agent", "prompt"), tool("WebFetch", "url")]);
}

let server;
let client;
let rates;

before(async () => {
  rates = mkdtempSync(join(tmpdir(), "traza-sdk-ts-rates-"));
  writeFileSync(join(rates, "pricing.json"), JSON.stringify(PRICING));
  server = await startServer({ args: ["--pricing", join(rates, "pricing.json")] });
  client = new Client(server.url);
  const calls = callsOf(plan([0, 1, 0, 2, 1]), "dr-0917");
  await client.request("POST", "/v1/spans", { body: spansOf({ run: "dr-0917", key: "dr-0917", start: T0, calls }) });
  // The deduping harness, tried as a rewind (live after step 11) and run on
  // live from its cut at 12, recorded under its own session with the fork's tags.
  const rewind = await client.replay("dr-0917", { liveAfter: 11 });
  const live = callsOf(plan([0, 1, 0, 2], { dedupe: true }), "dr-0917");
  await client.request("POST", "/v1/spans", {
    body: spansOf({ run: "dr-0917-dedupe", key: "dr-0917-dedupe", start: T0 + 90n * 60_000n * MS, calls: live, extra: rewind.fork.attributes }),
  });
});

after(async () => {
  await server?.stop();
  if (rates) rmSync(rates, { recursive: true, force: true });
});

describe("the research run the README and the guides print", () => {
  it("answers the README's replay loop from the recording", async () => {
    const traza = client;
    const replay = await traza.replay("dr-0917"); // strict by default
    const anthropic = new Anthropic({ baseURL: replay.anthropicBaseUrl, apiKey: traza.token ?? "traza-open" });
    assert.equal(await research(anthropic, (name) => replay.nextTool(name)), FINAL);
    const status = await replay.status();
    assert.deepEqual([status.served, status.remaining, status.diverged], [9, 0, null]);
    assert.deepEqual(status.calls.map((call) => call.step), [1, 4, 6, 7, 9, 11, 13, 15, 17]);
    assert.ok(status.calls.every((call) => call.matched));
  });

  it("diffs and recalls as the README shows", async () => {
    const diff = await client.diff("dr-0917", "dr-0917-dedupe");
    assert.equal(
      diff.first_divergence?.explanation,
      "step 12 of a and step 12 of b (tool:WebFetch) received the same input and produced a different output: response e450b5a73db1 in a, 750af04196bb in b",
    );
    assert.deepEqual(
      diff.aligned.slice(11).map((step) => `${step.op}${step.why ? `:${step.why}` : ""}`),
      ["same", "changed:decision", "changed:input", "same", "changed:input", "delete:structure", "delete:structure"],
    );
    assert.equal(diff.delta.steps, -2);
    assert.ok(Math.abs(diff.delta.cost_usd - -0.0682632) < 1e-9, String(diff.delta.cost_usd));
    const { runs } = await client.recall("solid-state battery", { outcome: "any", limit: 5 });
    assert.deepEqual(
      runs.map((run) => [run.run, run.outcome, run.score]),
      [
        ["dr-0917", "success", 11],
        ["dr-0917-dedupe", "success", 10],
      ],
    );
    assert.deepEqual(runs[0].skeleton.slice(0, 3), ["agent:research-coordinator", "llm:claude-opus-5-5", "tool:Agent"]);
  });

  it("fails the deduping harness's tape test where the fork guide says", async () => {
    const agent = (dedupe) => (replay) =>
      research(new Anthropic({ baseURL: replay.anthropicBaseUrl, apiKey: "traza" }), (name) => replay.nextTool(name), { dedupe });
    const failure = await replayTest(client, "dr-0917", agent(true)).then(
      () => assert.fail("the deduping harness passed"),
      (error) => error,
    );
    assert.ok(failure instanceof TapeMismatch, String(failure));
    assert.equal(
      failure.message,
      [
        "the agent diverged from recorded run dr-0917 at step 13:",
        "  message 9 (user): content differs at character 13",
        '    recorded: "[{\\"content\\":\\"Pilot lines (2026): three cell makers ran solid-state pilot lines at 0.2 to 1 GWh a year. First-pass yields…"',
        '    got:      "[{\\"content\\":\\"Already fetched https://www.example-energy.org/2026/solid-state-pilot-lines earlier in this run; reusing th…"',
        "  (the recording's request has 10 messages, the agent's 10)",
      ].join("\n"),
    );
    assert.equal(failure.cause?.status, 409);
    const status = await replayTest(client, "dr-0917", agent(false));
    assert.deepEqual([status.served, status.remaining], [9, 0]);
  });
});
