// Every client method against a real traza-server.
//
// Two servers serve the file: an open one (no credential, so every caller is
// an unbound admin) for the guard, tape, replay, diff, recall and ledger
// surfaces, and one started with TRAZA_TOKENS for the auth, quota and
// tenant-binding behaviour. Runs are seeded the way OpenLLMetry records them,
// through `client.request` — the client itself does not ingest. The tests run
// the compiled package (dist/), exactly what an application imports.

import assert from "node:assert/strict";
import { after, before, describe, it } from "node:test";
import { inspect } from "node:util";

import {
  AuthError,
  Client,
  Conflict,
  FuseTripped,
  NotFound,
  QuotaExceeded,
  RawNumber,
  TrazaError,
  otelEnv,
  parseJson,
  stringifyJson,
  verifyEvidenceDigest,
} from "../dist/index.js";
import { startServer } from "./server.js";
import { response, startStub } from "./stub.js";

// 2026-01-01T00:00:00Z, as exact bigint nanoseconds: old enough that every
// seeded run reads as finished.
const T0 = 1_767_225_600_000_000_000n;
const MS = 1_000_000n;

const ADMIN = "sdk-admin-token-0123456789abcdef";
const ACME_RO = "sdk-acme-reader-0123456789abcdef";
const ACME_RW = "sdk-acme-writer-0123456789abcdef";
const BETA_RO = "sdk-beta-reader-0123456789abcdef";

const SYSTEM = "You are a weather assistant.";

function span(trace, spanId, parent, name, startMs, endMs, attributes, { service = "weather-agent" } = {}) {
  return {
    trace_id: trace,
    span_id: spanId,
    parent_span_id: parent,
    name,
    service,
    status: "ok",
    start_time_ns: T0 + BigInt(startMs) * MS,
    end_time_ns: T0 + BigInt(endMs) * MS,
    attributes,
  };
}

/** OpenLLMetry's OpenAI instrumentation: a workflow, a model call asking for a tool, the tool, the answer. */
function weatherRun(session, trace, city, tenant) {
  const user = `What's the weather in ${city}?`;
  const args = JSON.stringify({ city });
  const result = JSON.stringify({ forecast: "rain", celsius: 12 });
  const llm = { "gen_ai.system": "openai", "gen_ai.request.model": "gpt-4o", "gen_ai.response.model": "gpt-4o-2024-08-06" };
  const spans = [
    span(trace, "wf", null, "weather_agent.workflow", 0, 1000, {
      "traceloop.span.kind": "workflow",
      "traceloop.entity.name": "weather_agent",
    }),
    span(trace, "llm-1", "wf", "openai.chat", 10, 400, {
      ...llm,
      "gen_ai.usage.prompt_tokens": 82,
      "gen_ai.usage.completion_tokens": 17,
      "gen_ai.prompt.0.role": "system",
      "gen_ai.prompt.0.content": SYSTEM,
      "gen_ai.prompt.1.role": "user",
      "gen_ai.prompt.1.content": user,
      "gen_ai.completion.0.role": "assistant",
      "gen_ai.completion.0.finish_reason": "tool_calls",
      "gen_ai.completion.0.tool_calls.0.id": "call_w1",
      "gen_ai.completion.0.tool_calls.0.name": "get_weather",
      "gen_ai.completion.0.tool_calls.0.arguments": args,
    }),
    span(trace, "tool-1", "wf", "get_weather.tool", 410, 450, {
      "traceloop.span.kind": "tool",
      "traceloop.entity.name": "get_weather",
      "traceloop.entity.input": args,
      "traceloop.entity.output": result,
    }),
    span(trace, "llm-2", "wf", "openai.chat", 460, 900, {
      ...llm,
      "gen_ai.usage.prompt_tokens": 120,
      "gen_ai.usage.completion_tokens": 14,
      "gen_ai.prompt.0.role": "system",
      "gen_ai.prompt.0.content": SYSTEM,
      "gen_ai.prompt.1.role": "user",
      "gen_ai.prompt.1.content": user,
      "gen_ai.prompt.2.role": "assistant",
      "gen_ai.prompt.2.tool_calls.0.id": "call_w1",
      "gen_ai.prompt.2.tool_calls.0.name": "get_weather",
      "gen_ai.prompt.2.tool_calls.0.arguments": args,
      "gen_ai.prompt.3.role": "tool",
      "gen_ai.prompt.3.tool_call_id": "call_w1",
      "gen_ai.prompt.3.content": result,
      "gen_ai.completion.0.role": "assistant",
      "gen_ai.completion.0.finish_reason": "stop",
      "gen_ai.completion.0.content": `It's raining in ${city}, 12°C. Take an umbrella.`,
    }),
  ];
  for (const item of spans) {
    item.attributes["session.id"] = session;
    if (tenant !== undefined) item.$tenant = tenant;
  }
  return spans;
}

/** `count` identical tool calls: what a looping agent records. */
function toolSteps(session, trace, count, { tenant, service = "loop-agent" } = {}) {
  return Array.from({ length: count }, (_, i) => {
    const item = span(
      trace,
      `t${i}`,
      null,
      "search_orders.tool",
      i * 10,
      i * 10 + 5,
      {
        "session.id": session,
        "gen_ai.operation.name": "execute_tool",
        "gen_ai.tool.name": "search_orders",
        "gen_ai.tool.call.arguments": JSON.stringify({ order: 42 }),
      },
      { service },
    );
    if (tenant !== undefined) item.$tenant = tenant;
    return item;
  });
}

/** The chat-completions body the OpenAI SDK sends for the weather agent. */
function openaiRequest(city, second = false) {
  const messages = [
    { role: "system", content: SYSTEM },
    { role: "user", content: `What's the weather in ${city}?` },
  ];
  if (second) {
    messages.push({
      role: "assistant",
      content: null,
      tool_calls: [{ id: "call_w1", type: "function", function: { name: "get_weather", arguments: JSON.stringify({ city }) } }],
    });
    messages.push({ role: "tool", tool_call_id: "call_w1", content: JSON.stringify({ forecast: "rain", celsius: 12 }) });
  }
  return { model: "gpt-4o", messages };
}

/** JSON with every object's keys reversed and spaces added, numbers verbatim. */
function reordered(value) {
  if (value === null || typeof value !== "object") return typeof value === "bigint" ? String(value) : JSON.stringify(value);
  if (value instanceof RawNumber) return value.text;
  if (Array.isArray(value)) return "[ " + value.map(reordered).join(", ") + " ]";
  return "{ " + Object.keys(value).reverse().map((key) => `${JSON.stringify(key)}: ${reordered(value[key])}`).join(", ") + " }";
}

/** A provider SDK's request, made with nothing but fetch. */
async function postJson(url, body, headers) {
  const response = await fetch(url, {
    method: "POST",
    headers: { "content-type": "application/json", ...headers },
    body: JSON.stringify(body),
  });
  return { status: response.status, body: await response.json() };
}

describe("@trazadb/sdk against traza-server", () => {
  let open;
  let auth;

  before(async () => {
    open = await startServer();
    auth = await startServer({
      env: { TRAZA_TOKENS: `admin:${ADMIN},ro@acme:${ACME_RO},rw@acme:${ACME_RW},ro@beta:${BETA_RO}` },
    });
    const seed = new Client(open.url);
    await seed.request("POST", "/v1/spans", { body: weatherRun("weather-7", "4bf92f3577b34da6a3ce929d0e0e4736", "Paris") });
    await seed.request("POST", "/v1/spans", { body: weatherRun("weather-8", "5cf92f3577b34da6a3ce929d0e0e4737", "Lyon") });
    // A metered cost the next day that the server prints as 2.0 and
    // JavaScript would re-print as 2: the digest check must keep the
    // server's text, or it would hash different bytes.
    const cost = span("6df92f3577b34da6a3ce929d0e0e4738", "cost", null, "openai.chat", 86_400_000, 86_400_100, {
      "gen_ai.system": "openai",
      "gen_ai.request.model": "gpt-4o",
      "llm.cost_usd": 2.0,
    });
    await seed.request("POST", "/v1/spans", { body: [cost] });
  });

  after(async () => {
    await open?.stop();
    await auth?.stop();
  });

  describe("identity and usage", () => {
    it("whoami on an open server", async () => {
      const me = await new Client(open.url).whoami();
      assert.deepEqual(me, { auth: "open", credential: null, operator: true, scope: "admin", tenant: null });
    });

    it("whoami names a bound credential", async () => {
      const me = await new Client(auth.url, { token: ACME_RW }).whoami();
      assert.equal(me.tenant, "acme");
      assert.equal(me.scope, "rw");
      assert.equal(me.operator, false);
      assert.match(me.credential.id, /^cr_/);
    });

    it("usage counts what was sent", async () => {
      const report = await new Client(open.url, { tenant: "" }).usage();
      assert.equal(typeof report.as_of_ns, "bigint");
      assert.equal(report.tenants.length, 1);
      const [row] = report.tenants;
      assert.equal(row.tenant, "");
      assert.ok(row.lifetime.spans >= 8);
      assert.equal(row.state, "ok");
      assert.match(row.month.window, /^\d{4}-\d{2}$/);
    });
  });

  describe("fuses", () => {
    it("check answers a hypothetical stop without tripping", async () => {
      const client = new Client(open.url, { tenant: "checks" });
      await client.putPolicy({ name: "default", limits: { max_cost_usd: 1.0 }, action: "stop" });
      const fresh = await client.check("fresh-run");
      assert.equal(fresh.verdict, "continue");
      assert.equal(fresh.policy, "default");
      const over = await client.check("fresh-run", { estCostUsd: 1.5, estTokens: 10, tool: "search" });
      assert.equal(over.verdict, "stop");
      assert.match(over.reasons[0], /would be exceeded by the next step/);
      assert.equal(over.tripped_by, null);
      assert.equal((await client.runVerdict("fresh-run")).verdict, "continue");
    });

    it("guard throws FuseTripped and the step never runs", async () => {
      const client = new Client(open.url, { tenant: "guarded" });
      await client.putPolicy({ name: "loop", limits: { max_repeats: 3 }, service: "loop-agent", action: "stop" });
      const ack = await client.request("POST", "/v1/spans", { body: toolSteps("loop-1", "a1".repeat(16), 3, { tenant: "guarded" }) });
      assert.equal(ack.fuses[0].verdict, "stop");

      const ran = [];
      const step = client.guarded("loop-1", async () => ran.push("step"), { tool: "search_orders" });
      const error = await step().then(
        () => assert.fail("the guard let a stopped run through"),
        (caught) => caught,
      );
      assert.ok(error instanceof FuseTripped);
      assert.ok(!(error instanceof TrazaError), "a stop is an answer, not a failed request");
      assert.deepEqual(ran, []);
      assert.equal(error.verdict.tripped_by, "policy");
      assert.equal(error.verdict.usage.top_repeat.count, 3);
      assert.match(error.message, /max_repeats 3 reached/);

      const verdict = await client.guard("loop-2");
      assert.equal(verdict.verdict, "continue");
    });

    it("guarded names the run from the wrapped function's arguments", async () => {
      const client = new Client(open.url, { tenant: "guarded" });
      await client.trip("node-stopped", { reason: "sdk decorator test" });
      const node = client.guarded((state) => state.runId, (state) => state.n + 1);
      assert.equal(await node({ runId: "node-ok", n: 1 }), 2);
      await assert.rejects(node({ runId: "node-stopped", n: 1 }), (error) => {
        assert.ok(error instanceof FuseTripped);
        assert.equal(error.verdict.tripped_by, "manual");
        return true;
      });
    });

    it("guard fails closed unless told to fail open", async () => {
      const unreachable = new Client("http://127.0.0.1:1", { timeoutMs: 2000 });
      await assert.rejects(unreachable.guard("any"), (error) => error instanceof TrazaError && error.status === null);
      const seen = [];
      const verdict = await unreachable.guard("any", { failOpen: true, onError: (error) => seen.push(error) });
      assert.equal(verdict, null);
      assert.equal(seen.length, 1);
      assert.ok(seen[0] instanceof TrazaError);
    });

    it("trip, reset, runs and the event feed", async () => {
      const client = new Client(open.url, { tenant: "ops" });
      const start = client.events();
      for await (const _ of start); // drain what the feed already holds for this tenant
      const cursor = start.cursor;
      assert.ok(cursor);

      const tripped = await client.trip("sess-9c21", { reason: "customer escalated" });
      assert.equal(tripped.verdict, "stop");
      assert.equal(tripped.tripped_by, "manual");
      assert.deepEqual(tripped.reasons, ["manually tripped: customer escalated"]);
      assert.equal(typeof tripped.tripped_at_ns, "bigint");
      assert.ok((await client.runs({ state: "stop" })).some((run) => run.run === "sess-9c21"));

      const reset = await client.reset("sess-9c21", { grant: { max_steps: 10 } });
      assert.equal(reset.verdict, "continue");
      assert.deepEqual(reset.grant, { max_steps: 10 });
      assert.ok(!(await client.runs({ state: "stop", limit: 50 })).some((run) => run.run === "sess-9c21"));

      const stream = client.events({ after: cursor });
      const kinds = [];
      for await (const event of stream) kinds.push([event.kind, event.run]);
      assert.deepEqual(kinds, [
        ["manual_stop", "sess-9c21"],
        ["reset", "sess-9c21"],
      ]);
      assert.equal(stream.gap, false);

      // A consumer that breaks after one event resumes at the next one.
      const partial = client.events({ after: cursor });
      for await (const event of partial) {
        assert.equal(event.kind, "manual_stop");
        break;
      }
      const rest = [];
      for await (const event of client.events({ after: partial.cursor })) rest.push(event.kind);
      assert.deepEqual(rest, ["reset"]);

      await client.trip("sess-9c21");
      const resumed = client.events({ after: stream.cursor });
      const later = [];
      for await (const event of resumed) later.push(event.kind);
      assert.deepEqual(later, ["manual_stop"]);

      // A long-poll with nothing new waits, then ends.
      const began = Date.now();
      for await (const _ of client.events({ after: resumed.cursor, waitMs: 300 })) assert.fail("no event expected");
      assert.ok(Date.now() - began >= 250);
    });

    it("a following stream long-polls unless told how long to wait", async () => {
      // A stub that answers every read at once with an empty page, as a quiet
      // feed would after its wait, and refuses the fifth so the endless
      // stream ends. Every read a follower makes must ask the server to hold
      // it open: with wait_ms=0 it would poll as fast as the network allows.
      const stub = await startStub(() =>
        stub.requests.length > 4
          ? response(500, '{"error":"enough"}')
          : response(200, '{"events":[],"next":"1.0"}', { "Content-Type": "application/json" }),
      );
      try {
        await assert.rejects(
          (async () => {
            for await (const _ of new Client(stub.url).events({ follow: true })) assert.fail("the stub sends no events");
          })(),
          TrazaError,
        );
        assert.deepEqual(stub.requests.map((r) => r.query.wait_ms), Array(5).fill("25000"));
        assert.deepEqual(stub.requests.map((r) => r.query.after), [undefined, "1.0", "1.0", "1.0", "1.0"]);
      } finally {
        await stub.close();
      }

      // A wait the caller names is kept; one that cannot wait is refused,
      // since following with it would be the same busy loop.
      const refusing = await startStub(() => response(500, "{}"));
      try {
        await assert.rejects(
          (async () => {
            for await (const _ of new Client(refusing.url).events({ follow: true, waitMs: 300 }));
          })(),
          TrazaError,
        );
        assert.equal(refusing.requests[0].query.wait_ms, "300");
      } finally {
        await refusing.close();
      }
      assert.throws(() => new Client(open.url).events({ follow: true, waitMs: 0 }), RangeError);

      // Without follow a read drains what is there and ends, so no wait.
      const draining = await startStub(() => response(200, '{"events":[]}'));
      try {
        for await (const _ of new Client(draining.url).events()) assert.fail("no event expected");
        assert.equal(draining.requests[0].query.wait_ms, "0");
      } finally {
        await draining.close();
      }
    });

    it("an aborted long-poll rejects with the caller's AbortError", async () => {
      const controller = new AbortController();
      const began = Date.now();
      setTimeout(() => controller.abort(), 100);
      const stream = new Client(open.url, { tenant: "abort" }).events({ waitMs: 20_000, follow: true, signal: controller.signal });
      await assert.rejects(
        (async () => {
          for await (const _ of stream);
        })(),
        (error) => error.name === "AbortError",
      );
      assert.ok(Date.now() - began < 5_000, "the abort cut the wait short");
    });

    it("a request past its timeout is a TrazaError with no status", async () => {
      const client = new Client(open.url, { tenant: "timeouts" });
      await assert.rejects(
        client.request("GET", "/v1/fuses/events", { query: { wait_ms: 3000, tenant: "timeouts" }, timeoutMs: 200 }),
        (error) => error instanceof TrazaError && error.status === null && /within 200 ms/.test(error.message),
      );
    });

    it("a run id is one path segment whatever it contains", async () => {
      const run = "team/a?x=1#frag %41";
      const client = new Client(open.url);
      await client.request("POST", "/v1/spans", { body: toolSteps(run, "b2".repeat(16), 1, { service: "slash-agent" }) });
      const verdict = await client.runVerdict(run);
      assert.equal(verdict.run, run);
      assert.equal(verdict.usage.tool_calls, 1);
      assert.equal((await client.trip(run)).run, run);
      assert.equal((await client.tape(run)).steps[0].tool, "search_orders");
    });

    it("policies: create, replace, list and delete", async () => {
      const client = new Client(open.url, { tenant: "policies" });
      const created = await client.putPolicy({
        name: "support",
        service: "support-agent",
        limits: { max_cost_usd: 2.0, max_steps: 40, max_repeats: 4 },
        action: "stop",
      });
      assert.match(created.id, /^fp_/);
      assert.deepEqual(created.match, { service: "support-agent" });
      const replaced = await client.putPolicy({ name: "support", service: "support-agent", limits: { max_steps: 80 }, action: "stop" });
      assert.equal(replaced.id, created.id);
      assert.deepEqual(replaced.limits, { max_steps: 80 });
      const listing = await client.policies();
      assert.deepEqual(listing.policies.map((p) => p.name), ["support"]);
      assert.equal(listing.global.name, "global");
      assert.equal((await client.deletePolicy(created.id)).id, created.id);
      assert.deepEqual((await client.policies()).policies, []);
      await assert.rejects(client.deletePolicy(created.id), NotFound);
    });
  });

  describe("tapes and replays", () => {
    it("tape, with exact nanosecond timestamps", async () => {
      const client = new Client(open.url);
      const tape = await client.tape("weather-7");
      assert.deepEqual(tape.steps.map((s) => s.kind), ["llm", "tool", "llm"]);
      const [first, tool] = tape.steps;
      assert.equal(first.start_ns, T0 + 10n * MS, "a double would have rounded this");
      assert.equal(first.duration_ns, 390n * MS);
      assert.deepEqual(first.request[1], { role: "user", content: "What's the weather in Paris?" });
      assert.deepEqual(first.tokens, { input: 82, output: 17, cache_read: null, cache_write: null });
      assert.deepEqual(tool.response, { celsius: 12, forecast: "rain" });
      assert.equal(tape.spans_examined, 4);
      assert.equal((await client.tape("weather-7", { maxSteps: 1 })).truncated, true);
      await assert.rejects(client.tape("no-such-run"), NotFound);
    });

    it("a replay consumed by plain fetch on the OpenAI route", async () => {
      const client = new Client(open.url);
      const replay = await client.replay("weather-7");
      assert.match(replay.id, /^rp_/);
      assert.equal(replay.llm_steps, 2);
      const env = replay.env();
      assert.equal(env.OPENAI_BASE_URL, `${open.url}/v1/replays/${replay.id}/openai/v1`);
      assert.equal(env.ANTHROPIC_BASE_URL, `${open.url}/v1/replays/${replay.id}/anthropic`);
      assert.ok(env.OPENAI_API_KEY);

      const headers = { authorization: `Bearer ${env.OPENAI_API_KEY}` };
      const first = await postJson(`${env.OPENAI_BASE_URL}/chat/completions`, openaiRequest("Paris"), headers);
      assert.equal(first.status, 200);
      const call = first.body.choices[0].message.tool_calls[0];
      assert.equal(call.function.name, "get_weather");
      assert.deepEqual(JSON.parse(call.function.arguments), { city: "Paris" });

      const tool = await replay.nextTool("get_weather");
      assert.deepEqual(tool, { step: 1, tool: "get_weather", arguments: { city: "Paris" }, result: { celsius: 12, forecast: "rain" } });
      assert.equal(await replay.nextTool(), null);

      const second = await postJson(`${env.OPENAI_BASE_URL}/chat/completions`, openaiRequest("Paris", true), headers);
      assert.equal(second.body.choices[0].message.content, "It's raining in Paris, 12°C. Take an umbrella.");

      const status = await replay.status();
      assert.equal(status.served, 2);
      assert.equal(status.remaining, 0);
      assert.equal(status.diverged, null);
      assert.deepEqual(status.calls.map((c) => [c.step, c.matched]), [
        [0, true],
        [2, true],
      ]);
      const exhausted = await postJson(`${env.OPENAI_BASE_URL}/chat/completions`, openaiRequest("Paris"), headers);
      assert.equal(exhausted.status, 410);
    });

    it("a strict divergence is a Conflict that says not to retry", async () => {
      const client = new Client(open.url);
      const replay = await client.replay("weather-7");
      await assert.rejects(
        client.request("POST", `${replay.base_urls.openai}/chat/completions`, { body: openaiRequest("Lyon") }),
        (error) => error instanceof Conflict && error.type === "replay_divergence" && error.retryable === false,
      );
      assert.equal((await replay.status()).diverged.step, 0);
      const loose = await client.replay("weather-7", { mode: "loose" });
      const answer = await client.request("POST", `${loose.base_urls.openai}/chat/completions`, { body: openaiRequest("Lyon") });
      assert.equal(answer.choices[0].finish_reason, "tool_calls");
    });

    it("a bound reader replays through the Anthropic route", async () => {
      await new Client(auth.url, { token: ACME_RW }).request("POST", "/v1/spans", {
        body: weatherRun("acme-weather", "c3".repeat(16), "Oslo"),
      });
      const replay = await new Client(auth.url, { token: ACME_RO }).replay("acme-weather");
      const env = replay.env();
      assert.equal(env.ANTHROPIC_API_KEY, ACME_RO);
      const message = await postJson(
        `${env.ANTHROPIC_BASE_URL}/v1/messages`,
        { model: "claude-x", max_tokens: 256, system: SYSTEM, messages: [{ role: "user", content: "What's the weather in Oslo?" }] },
        { "x-api-key": env.ANTHROPIC_API_KEY, "anthropic-version": "2023-06-01" },
      );
      assert.equal(message.status, 200);
      assert.equal(message.body.stop_reason, "tool_use");
      assert.deepEqual(message.body.content[0].input, { city: "Oslo" });
    });

    it("env can hand the agent its own key", async () => {
      // The operator creates the replay under the run's tenant, and the agent
      // process gets a bound reader's key instead of the operator's: every
      // library in that process can read what env() exports.
      await new Client(auth.url, { token: ACME_RW }).request("POST", "/v1/spans", {
        body: weatherRun("acme-env", "c4".repeat(16), "Bergen"),
      });
      const replay = await new Client(auth.url, { token: ADMIN, tenant: "acme" }).replay("acme-env");
      const env = replay.env({ apiKey: ACME_RO });
      assert.equal(env.OPENAI_API_KEY, ACME_RO);
      assert.equal(env.ANTHROPIC_API_KEY, ACME_RO);
      assert.ok(!Object.values(env).includes(ADMIN));
      assert.equal(env.OPENAI_BASE_URL, replay.openaiBaseUrl);
      const answer = await postJson(`${env.OPENAI_BASE_URL}/chat/completions`, openaiRequest("Bergen"), {
        authorization: `Bearer ${env.OPENAI_API_KEY}`,
      });
      assert.equal(answer.status, 200);
      assert.equal(answer.body.choices[0].finish_reason, "tool_calls");
      // Without a key the client's own credential stands in, as before.
      assert.equal(replay.env().OPENAI_API_KEY, ADMIN);
    });
  });

  describe("diff and recall", () => {
    it("diff names the first divergence", async () => {
      const client = new Client(open.url);
      const diff = await client.diff("weather-7", "weather-8");
      assert.deepEqual(diff.aligned.map((s) => [s.op, s.why]), Array(3).fill(["changed", "input"]));
      assert.equal(diff.first_divergence.kind, "input");
      assert.equal(diff.delta.steps, 0);
      assert.equal((await client.diff("weather-7", "weather-7")).first_divergence, null);
      await assert.rejects(client.diff("weather-7", "no-such-run"), NotFound);
    });

    it("recall ranks earlier runs", async () => {
      const client = new Client(open.url);
      const found = await client.recall("weather Paris");
      assert.deepEqual(found.runs.map((r) => r.run), ["weather-7"]);
      assert.equal(found.runs[0].outcome, "success");
      assert.deepEqual(found.runs[0].skeleton, ["llm:gpt-4o-2024-08-06", "tool:get_weather", "llm:gpt-4o-2024-08-06"]);
      assert.equal((await client.recall("weather", { limit: 1 })).runs.length, 1);
      assert.equal((await client.recall("weather Paris", { outcome: "failure" })).runs.length, 0);
    });
  });

  describe("ledger", () => {
    it("ledger head, exact to the nanosecond", async () => {
      const head = await new Client(open.url).ledgerHead();
      const raw = await (await fetch(`${open.url}/v1/ledger/head`)).text();
      assert.equal(head.at_ns, BigInt(/"at_ns":(\d+)/.exec(raw)[1]));
      assert.match(head.head, /^[0-9a-f]{64}$/);
      assert.ok(head.seq >= 1);
    });

    it("evidence pack and its digest", async () => {
      const client = new Client(open.url);
      await client.request("POST", "/v1/checkpoint");
      const pack = await client.evidence();
      assert.equal(pack.format, "traza-evidence/1");
      // The engine's version, whatever this package's is: the two are kept in
      // step by scripts/check-sdk-metadata.py at the bump, not here.
      assert.match(pack.engine_version, /^\d+\.\d+\.\d+/);
      assert.ok(pack.activity.spans >= 8);
      assert.equal(pack.ledger.verification.intact, true);
      assert.equal(await pack.verifyDigest(), true);

      // The check is over the bytes received: change one and it fails.
      const text = await (await fetch(`${open.url}/v1/evidence`)).text();
      assert.equal(await verifyEvidenceDigest(text), true);
      assert.match(text, /"cost_usd":2\.0,/, "the number JavaScript would re-print differently");
      assert.equal(await verifyEvidenceDigest(text.replace('"format":"traza-evidence/1"', '"format":"traza-evidence/2"')), false);
      // A pack saved in another key order, with whitespace, still verifies:
      // the digest is over the canonical form, not the bytes as stored.
      assert.equal(await verifyEvidenceDigest(reordered(parseJson(text, { rawNumbers: true }))), true);

      const window = await client.evidence({ since: 0, until: 1n });
      assert.deepEqual(window.window, { since_ns: 0n, until_ns: 1n });
      assert.equal(await window.verifyDigest(), true);

      const bound = await new Client(auth.url, { token: ACME_RO }).evidence();
      assert.equal(bound.tenant, "acme");
      assert.equal(await bound.verifyDigest(), true);
    });
  });

  describe("auth and quotas", () => {
    it("status codes map to errors", async () => {
      await assert.rejects(new Client(auth.url).whoami(), (error) => error instanceof AuthError && error.status === 401);
      await assert.rejects(
        new Client(auth.url, { token: ACME_RO }).trip("r1"),
        (error) => error instanceof AuthError && error.status === 403 && error.required === "rw",
      );
      await assert.rejects(
        new Client(auth.url, { token: ACME_RO, tenant: "beta" }).runVerdict("r1"),
        (error) => error instanceof AuthError && error.message === "this credential is bound to a different tenant",
      );
      assert.equal((await new Client(auth.url, { token: ACME_RO }).check("r1")).tenant, "acme");
    });

    it("a quota refusal carries Retry-After and the quota", async () => {
      const admin = new Client(auth.url, { token: ADMIN });
      await admin.request("POST", "/v1/admin/tenants/quota-co/limits", { body: { monthly_spans: 2 } });
      await assert.rejects(
        admin.request("POST", "/v1/spans", { body: toolSteps("q1", "d4".repeat(16), 3, { tenant: "quota-co" }) }),
        (error) => {
          assert.ok(error instanceof QuotaExceeded);
          assert.ok(error.retryAfter > 0);
          assert.equal(error.quota.dimension, "monthly_spans");
          assert.equal(error.quota.limit, 2);
          return true;
        },
      );
      const [row] = (await new Client(auth.url, { token: ADMIN, tenant: "quota-co" }).usage()).tenants;
      assert.equal(row.lifetime.rejected, 3);
    });

    it("a full replay share is a 429 without a quota", async () => {
      await new Client(auth.url, { token: ADMIN }).request("POST", "/v1/spans", {
        body: weatherRun("beta-weather", "e5".repeat(16), "Rome", "beta"),
      });
      const reader = new Client(auth.url, { token: BETA_RO });
      for (let i = 0; i < 64; i++) await reader.replay("beta-weather");
      await assert.rejects(reader.replay("beta-weather"), (error) => {
        assert.ok(error instanceof QuotaExceeded);
        assert.ok(error.retryAfter > 0);
        assert.equal(error.quota, null);
        return true;
      });
    });
  });

  describe("otelEnv", () => {
    it("writes the trace exporter variables", () => {
      assert.deepEqual(otelEnv("http://traza.internal:8080/", { token: "trz_abc", tenant: "acme" }), {
        OTEL_EXPORTER_OTLP_TRACES_ENDPOINT: "http://traza.internal:8080/v1/traces",
        OTEL_EXPORTER_OTLP_TRACES_PROTOCOL: "http/protobuf",
        OTEL_EXPORTER_OTLP_TRACES_HEADERS: "Authorization=Bearer%20trz_abc",
        OTEL_RESOURCE_ATTRIBUTES: "traza.tenant=acme",
      });
    });

    it("its endpoint, headers and tenant reach the server", async () => {
      // An exporter decodes the headers variable and posts to the endpoint
      // as given; OTLP/JSON stands in for protobuf, and the route takes both.
      const env = otelEnv(auth.url, { token: ADMIN, tenant: "otel-co" });
      const [name, value] = env.OTEL_EXPORTER_OTLP_TRACES_HEADERS.split("=");
      const [key, tenant] = env.OTEL_RESOURCE_ATTRIBUTES.split("=");
      const exported = await postJson(
        env.OTEL_EXPORTER_OTLP_TRACES_ENDPOINT,
        {
          resourceSpans: [
            {
              resource: { attributes: [{ key, value: { stringValue: tenant } }] },
              scopeSpans: [
                {
                  spans: [
                    {
                      traceId: "f6".repeat(16),
                      spanId: "f6".repeat(8),
                      name: "otel-span",
                      startTimeUnixNano: String(T0),
                      endTimeUnixNano: String(T0 + MS),
                    },
                  ],
                },
              ],
            },
          ],
        },
        { [decodeURIComponent(name)]: decodeURIComponent(value) },
      );
      assert.deepEqual(exported, { status: 200, body: { partialSuccess: {} } });
      const [row] = (await new Client(auth.url, { token: ADMIN, tenant: "otel-co" }).usage()).tenants;
      assert.equal(row.lifetime.spans, 1);
    });
  });

  describe("transport", () => {
    it("a redirect is refused and the token goes nowhere", async () => {
      // Something in front of Traza (a proxy upgrading the scheme or
      // canonicalising the host) redirects. Traza never redirects its own
      // routes, so the client follows none, whatever its fetch would do.
      const elsewhere = await startStub(() => response(200, "{}"));
      try {
        const location = `http://127.0.0.1:${elsewhere.port}/steal`;
        for (const status of [301, 302, 303, 307, 308]) {
          const front = await startStub(() => response(status, "moved", { Location: location }));
          try {
            const client = new Client(front.url, { token: "trz_secret_token_0123456789" });
            for (const call of [() => client.ledgerHead(), () => client.check("r1")]) {
              await assert.rejects(call(), (error) => {
                assert.ok(error instanceof TrazaError, String(error));
                assert.equal(error.status, status);
                assert.ok(error.message.includes(location), error.message);
                return true;
              });
            }
            assert.equal(front.requests.length, 2);
          } finally {
            await front.close();
          }
        }
        assert.deepEqual(elsewhere.requests, [], "nothing followed the redirect");
      } finally {
        await elsewhere.close();
      }
    });

    it("a path not starting with a slash is refused before the token is attached", async () => {
      // The path is appended to the client's URL, so one that does not start
      // with "/" continues the host instead of starting the path:
      // "https://traza.example" and ".attacker.example/v1/whoami" make a
      // request to traza.example.attacker.example, token and all, with no
      // redirect involved. A caller forwarding a route it was handed would
      // give its credential away. A recording fetch sees every URL the client
      // asks for, whatever host it names, without any name having to resolve.
      const token = "trz_secret_token_0123456789";
      const sent = [];
      const fetch = async (url, init) => {
        sent.push({ url, authorization: init.headers.authorization });
        return new Response("{}", { status: 200 });
      };
      const client = new Client("https://traza.example", { token, fetch });
      // What a permitted request looks like, so the silence below means
      // nothing was sent.
      assert.deepEqual(await client.request("GET", "/v1/whoami"), {});
      assert.deepEqual(sent, [{ url: "https://traza.example/v1/whoami", authorization: `Bearer ${token}` }]);
      for (const path of [
        ".attacker.example/v1/whoami",
        "@attacker.example/v1/whoami",
        ":8443/v1/whoami",
        "v1/whoami",
        "",
        "https://attacker.example/v1/whoami",
      ]) {
        await assert.rejects(client.request("GET", path), TypeError, JSON.stringify(path));
      }
      assert.deepEqual(sent.slice(1), [], "a refused path sent nothing anywhere");
    });

    it("an unreadable answer is a TrazaError, and failOpen covers it", async () => {
      // A reverse proxy's sign-in page, a captive portal, a JSON value that is
      // not the object the route answers, a check with no verdict, a body cut
      // short, a status line that is not HTTP: each is an answer the guard
      // cannot act on, so it must fail closed by default and open when asked,
      // exactly like an unreachable server.
      const json = { "Content-Type": "application/json" };
      const answers = {
        html: response(200, "<html>Sign in</html>", { "Content-Type": "text/html" }),
        array: response(200, "[]", json),
        empty: response(200),
        "no verdict": response(200, '{"run":"r1"}', json),
        "unknown verdict": response(200, '{"run":"r1","verdict":"maybe"}', json),
        truncated: Buffer.from('HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n{"verd'),
        "garbage status": Buffer.from("HELLO\r\n\r\n"),
      };
      for (const [name, raw] of Object.entries(answers)) {
        const stub = await startStub(() => raw);
        try {
          const client = new Client(stub.url, { timeoutMs: 5000 });
          await assert.rejects(client.guard("r1"), (error) => {
            assert.ok(error instanceof TrazaError, `${name}: ${error}`);
            return true;
          });
          const seen = [];
          assert.equal(await client.guard("r1", { failOpen: true, onError: (error) => seen.push(error) }), null, name);
          assert.equal(seen.length, 1, name);
          assert.ok(seen[0] instanceof TrazaError, name);
        } finally {
          await stub.close();
        }
      }
      // When the status is known it is kept, with the text that arrived.
      const stub = await startStub(() => answers.html);
      try {
        await assert.rejects(new Client(stub.url).ledgerHead(), (error) => {
          assert.ok(error instanceof TrazaError, String(error));
          assert.equal(error.status, 200);
          assert.equal(error.body, "<html>Sign in</html>");
          return true;
        });
        await assert.rejects(new Client(stub.url).request("GET", "/v1/anything"), TrazaError);
        await assert.rejects(new Client(stub.url).evidence(), TrazaError);
      } finally {
        await stub.close();
      }
    });
  });

  describe("configuration and JSON", () => {
    it("fromEnv, and the token never printed", () => {
      const client = Client.fromEnv({ TRAZA_URL: "http://h:1/", TRAZA_TOKEN: "secret-token", TRAZA_TENANT: "acme" });
      assert.equal(client.url, "http://h:1");
      assert.equal(client.token, "secret-token");
      assert.equal(client.tenant, "acme");
      assert.ok(!inspect(client).includes("secret-token"));
      assert.ok(!JSON.stringify(client).includes("secret-token"));
      assert.throws(() => new Client("127.0.0.1:8080"), TypeError);
    });

    it("an empty TRAZA_TENANT is unset", async () => {
      // As the shell hook reads it: `TRAZA_TENANT=` left in a profile must
      // not name the default tenant, which a bound credential is refused (and
      // which would narrow an unbound one's reads to it).
      const client = Client.fromEnv({ TRAZA_URL: auth.url, TRAZA_TOKEN: ACME_RO, TRAZA_TENANT: "" });
      assert.equal(client.tenant, undefined);
      assert.equal((await client.check("r1")).tenant, "acme");
      // An explicit "" still names the default tenant, which this credential
      // may not ask about.
      await assert.rejects(new Client(auth.url, { token: ACME_RO, tenant: "" }).check("r1"), AuthError);
      assert.equal(Client.fromEnv({ TRAZA_TENANT: "" }).tenant, undefined);
    });

    it("reads nanoseconds and large integers exactly, payloads as JSON.parse does", () => {
      const parsed = parseJson(
        '{"at_ns":1792061561108805000,"small_ns":5,"seq":3,"big":9007199254740993,"cost":0.5,' +
          '"request":{"timeout_ns":5,"n":1},"__proto__":{"polluted":true}}',
      );
      assert.equal(parsed.at_ns, 1792061561108805000n);
      assert.equal(parsed.small_ns, 5n);
      assert.equal(parsed.seq, 3);
      assert.equal(parsed.big, 9007199254740993n);
      assert.equal(parsed.cost, 0.5);
      assert.deepEqual(parsed.request, { timeout_ns: 5, n: 1 });
      assert.equal({}.polluted, undefined);
      assert.equal(Object.getPrototypeOf(parsed), Object.prototype);
      assert.deepEqual(parsed.__proto__, { polluted: true });
      assert.throws(() => parseJson('{"a":1,}'), SyntaxError);
      assert.equal(stringifyJson({ t: 1792061561108805000n, u: undefined, a: [undefined, 1] }), '{"t":1792061561108805000,"a":[null,1]}');
    });

    it("writes a sparse array's holes as null, as JSON.stringify does", () => {
      // Array.prototype.map skips holes: written that way, [1, , 3] came out
      // as "[1,,3]", which no parser accepts, and new Array(2) as "[]", which
      // drops elements an application sent.
      const holes = [1, , 3];
      assert.equal(stringifyJson(holes), "[1,null,3]");
      assert.equal(stringifyJson(holes), JSON.stringify(holes));
      assert.equal(stringifyJson(new Array(2)), "[null,null]");
      assert.equal(stringifyJson({ steps: [, 7n] }), '{"steps":[null,7]}');
    });
  });
});
