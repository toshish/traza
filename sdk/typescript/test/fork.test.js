// Forks, tape tests and golden runs against a real traza-server.
//
// The weather agent's run is seeded as OpenLLMetry records it. The "real
// provider" a fork goes live to is a canned-answer stub on loopback, so the
// switching fetch's hand-over is observed exactly — which request reached
// it, with which key — and nothing ever leaves the machine. The tests run the
// compiled package (dist/), exactly what an application imports.

import assert from "node:assert/strict";
import { after, before, describe, it } from "node:test";

import { Client, Fork, TapeMismatch, describeReplay, replayTest, stringifyJson } from "../dist/index.js";
import { startServer } from "./server.js";
import { response, startStub } from "./stub.js";

const T0 = 1_767_225_600_000_000_000n;
const MS = 1_000_000n;
const SYSTEM = "You are a weather assistant.";
const USER = "What's the weather in Paris?";
const RAIN = JSON.stringify({ forecast: "rain", celsius: 12 });

function span(trace, spanId, name, startMs, endMs, attributes) {
  return {
    trace_id: trace,
    span_id: spanId,
    parent_span_id: null,
    name,
    service: "weather-agent",
    status: "ok",
    start_time_ns: T0 + BigInt(startMs) * MS,
    end_time_ns: T0 + BigInt(endMs) * MS,
    attributes,
  };
}

function weatherRun(session, trace, { atMs = 0, result = RAIN, extra = {} } = {}) {
  const llm = { "gen_ai.system": "openai", "gen_ai.request.model": "gpt-4o", "gen_ai.response.model": "gpt-4o-2024-08-06" };
  const args = JSON.stringify({ city: "Paris" });
  const spans = [
    span(trace, "llm-1", "openai.chat", atMs, atMs + 400, {
      ...llm,
      "gen_ai.prompt.0.role": "system",
      "gen_ai.prompt.0.content": SYSTEM,
      "gen_ai.prompt.1.role": "user",
      "gen_ai.prompt.1.content": USER,
      "gen_ai.completion.0.role": "assistant",
      "gen_ai.completion.0.finish_reason": "tool_calls",
      "gen_ai.completion.0.tool_calls.0.id": "call_w1",
      "gen_ai.completion.0.tool_calls.0.name": "get_weather",
      "gen_ai.completion.0.tool_calls.0.arguments": args,
    }),
    span(trace, "tool-1", "get_weather.tool", atMs + 410, atMs + 450, {
      "traceloop.span.kind": "tool",
      "traceloop.entity.name": "get_weather",
      "traceloop.entity.input": args,
      "traceloop.entity.output": result,
    }),
    span(trace, "llm-2", "openai.chat", atMs + 460, atMs + 900, {
      ...llm,
      "gen_ai.prompt.0.role": "system",
      "gen_ai.prompt.0.content": SYSTEM,
      "gen_ai.prompt.1.role": "user",
      "gen_ai.prompt.1.content": USER,
      "gen_ai.prompt.2.role": "assistant",
      "gen_ai.prompt.2.tool_calls.0.id": "call_w1",
      "gen_ai.prompt.2.tool_calls.0.name": "get_weather",
      "gen_ai.prompt.2.tool_calls.0.arguments": args,
      "gen_ai.prompt.3.role": "tool",
      "gen_ai.prompt.3.tool_call_id": "call_w1",
      "gen_ai.prompt.3.content": result,
      "gen_ai.completion.0.role": "assistant",
      "gen_ai.completion.0.finish_reason": "stop",
      "gen_ai.completion.0.content": "It's raining in Paris, 12°C.",
    }),
  ];
  for (const item of spans) Object.assign(item.attributes, { "session.id": session }, extra);
  return spans;
}

function messages({ user = USER, toolResult } = {}) {
  const out = [
    { role: "system", content: SYSTEM },
    { role: "user", content: user },
  ];
  if (toolResult !== undefined) {
    out.push({
      role: "assistant",
      content: null,
      tool_calls: [{ id: "call_w1", type: "function", function: { name: "get_weather", arguments: '{"city":"Paris"}' } }],
    });
    out.push({ role: "tool", tool_call_id: "call_w1", content: toolResult });
  }
  return out;
}

/** What the OpenAI SDK does with `baseURL`, `apiKey` and `fetch`. */
async function chat({ baseURL, apiKey, fetch }, body) {
  const answer = await fetch(`${baseURL}/chat/completions`, {
    method: "POST",
    headers: { "content-type": "application/json", authorization: `Bearer ${apiKey}` },
    body: JSON.stringify(body),
  });
  return { status: answer.status, body: await answer.json() };
}

/** What the Anthropic SDK does. */
async function messagesCall({ baseURL, apiKey, fetch }, body) {
  const answer = await fetch(`${baseURL}/v1/messages`, {
    method: "POST",
    headers: { "content-type": "application/json", "x-api-key": apiKey, "anthropic-version": "2023-06-01" },
    body: JSON.stringify(body),
  });
  return { status: answer.status, body: await answer.json() };
}

function providerAnswer(request) {
  const body =
    request.path === "/v1/messages"
      ? { id: "msg_live", type: "message", role: "assistant", model: "claude-live", content: [{ type: "text", text: "Live." }], stop_reason: "end_turn" }
      : {
          id: "chatcmpl-live",
          object: "chat.completion",
          created: 1,
          model: "gpt-4o-live",
          choices: [{ index: 0, finish_reason: "stop", message: { role: "assistant", content: "Live: it is sunny in Paris." } }],
        };
  return response(200, JSON.stringify(body), { "Content-Type": "application/json" });
}

let server;
let client;

before(async () => {
  server = await startServer();
  client = new Client(server.url);
  await client.request("POST", "/v1/spans", { body: weatherRun("weather-7", "4bf92f3577b34da6a3ce929d0e0e4736") });
  await client.request("POST", "/v1/spans", {
    body: weatherRun("golden-run", "4bf92f3577b34da6a3ce929d0e0e4737", { extra: { "traza.golden": true } }),
  });
});

after(async () => {
  await server?.stop();
});

describe("a fork", () => {
  it("replays up to the cut, serves the edit, then switches to the provider", async () => {
    const provider = await startStub(providerAnswer);
    try {
      const fork = await client.fork("weather-7", {
        atStep: 1,
        toolResult: { forecast: "sun", celsius: 30 },
        liveOpenAI: { baseURL: `${provider.url}/v1`, apiKey: "sk-real-provider-key" },
      });
      assert.ok(fork instanceof Fork);
      assert.match(fork.id, /^fk_[0-9a-f]{32}$/);
      assert.equal(fork.step, 1);
      assert.deepEqual(fork.attributes, { "traza.fork.parent": "weather-7", "traza.fork.step": 1, "traza.fork.id": fork.id });
      const openai = fork.openaiOptions();
      assert.ok(openai.baseURL.endsWith("/openai/v1"));

      const first = await chat(openai, { model: "gpt-4o", messages: messages() });
      assert.equal(first.status, 200);
      assert.equal(first.body.choices[0].message.tool_calls[0].id, "call_w1");
      const tool = await fork.toolResult("get_weather");
      assert.deepEqual(tool.result, { forecast: "sun", celsius: 30 });
      assert.equal(tool.edited, true);
      assert.equal(provider.requests.length, 0, "nothing has gone live yet");

      const edited = JSON.stringify(tool.result);
      const live = await chat(openai, { model: "gpt-4o", messages: messages({ toolResult: edited }) });
      assert.equal(live.status, 200);
      assert.equal(live.body.choices[0].message.content, "Live: it is sunny in Paris.");
      assert.equal(fork.isLive, true);
      assert.equal(provider.requests.length, 1);
      const sent = provider.requests[0];
      assert.equal(sent.path, "/v1/chat/completions");
      assert.equal(sent.headers.authorization, "Bearer sk-real-provider-key");
      assert.equal(JSON.parse(sent.body).messages[3].content, edited, "the provider sees the edited conversation");
      assert.equal(await fork.callTool("get_weather", () => "ran live"), "ran live");

      const status = await fork.status();
      assert.equal(status.fork_id, fork.id);
      assert.deepEqual(status.fork.edits, [{ step: 1, kind: "tool_result", served: true }]);
      assert.equal(status.fork.live_calls, 2);
      fork.close();
    } finally {
      await provider.close();
    }
  });

  it("switches an Anthropic SDK too, with its own key header", async () => {
    const provider = await startStub(providerAnswer);
    try {
      const fork = await client.fork("weather-7", {
        atStep: 0,
        response: "It is sunny in Paris.",
        liveAnthropic: { baseURL: provider.url, apiKey: "sk-ant-real" },
      });
      const anthropic = fork.anthropicOptions();
      const body = { model: "claude-x", max_tokens: 64, system: SYSTEM, messages: [{ role: "user", content: USER }] };
      const edited = await messagesCall(anthropic, body);
      assert.equal(edited.status, 200);
      assert.deepEqual(edited.body.content, [{ type: "text", text: "It is sunny in Paris." }]);
      const live = await messagesCall(anthropic, { ...body, messages: [...body.messages, { role: "assistant", content: "x" }] });
      assert.equal(live.body.id, "msg_live");
      assert.equal(provider.requests[0].headers["x-api-key"], "sk-ant-real");
      assert.equal(provider.requests[0].headers.authorization, undefined);
    } finally {
      await provider.close();
    }
  });

  it("without live, serves the edit and lets the recording play on", async () => {
    const fork = await client.fork("weather-7", { atStep: 0, response: "Sunny.", live: false });
    const openai = fork.openaiOptions();
    assert.equal((await chat(openai, { model: "gpt-4o", messages: messages() })).body.choices[0].message.content, "Sunny.");
    const next = await chat(openai, { model: "gpt-4o", messages: messages({ toolResult: "anything" }) });
    assert.equal(next.body.choices[0].message.content, "It's raining in Paris, 12°C.");
    assert.equal(fork.isLive, false);
  });

  it("checks its arguments before anything is sent", () => {
    assert.throws(() => Fork.spec({ atStep: -1 }), RangeError);
    assert.throws(() => Fork.spec({ atStep: 0 }), RangeError);
    assert.throws(() => Fork.spec({ atStep: 1, live: false }), TypeError);
    assert.throws(() => Fork.spec({ atStep: 1, response: "a", toolResult: "b" }), TypeError);
    assert.deepEqual(Fork.spec({ atStep: 2 }), { edits: [], liveAfter: 1 });
    assert.deepEqual(Fork.spec({ atStep: 1, toolResult: null }), { edits: [{ step: 1, tool_result: null }], liveAfter: 1 });
  });

  it("tags spans while open, and is found again in the run's forks", async () => {
    const fork = await client.fork("weather-7", { atStep: 1, toolResult: "sunny", live: false, runId: "weather-7-sunny" });
    const processor = fork.spanProcessor();
    const tagged = { attributes: {}, setAttribute(key, value) { this.attributes[key] = value; } };
    processor.onStart(tagged);
    assert.deepEqual(tagged.attributes, {
      "traza.fork.parent": "weather-7",
      "traza.fork.step": 1,
      "traza.fork.id": fork.id,
      "session.id": "weather-7-sunny",
    });
    fork.close();
    const late = { attributes: {}, setAttribute(key, value) { this.attributes[key] = value; } };
    processor.onStart(late);
    assert.deepEqual(late.attributes, {}, "a closed fork tags nothing");

    await client.request("POST", "/v1/spans", {
      body: weatherRun(fork.runId, "4bf92f3577b34da6a3ce929d0e0e4738", { atMs: 5000, result: "sunny", extra: tagged.attributes }),
    });
    const listing = await client.forks("weather-7");
    const mine = listing.forks.find((item) => item.fork_id === fork.id);
    assert.ok(mine, stringifyJson(listing));
    assert.equal(mine.run, "weather-7-sunny");
    assert.deepEqual([mine.first_divergence.a, mine.first_divergence.b, mine.first_divergence.kind], [1, 1, "decision"]);
    assert.equal((await fork.diff()).first_divergence.b, 1);
  });
});

describe("replayTest", () => {
  it("passes an agent that takes the recorded path", async () => {
    const status = await replayTest(client, "weather-7", async (replay) => {
      const openai = { baseURL: replay.openaiBaseUrl, apiKey: "traza", fetch };
      await chat(openai, { model: "gpt-4o", messages: messages() });
      await chat(openai, { model: "gpt-4o", messages: messages({ toolResult: RAIN }) });
    });
    assert.equal(status.served, 2);
    assert.equal(status.remaining, 0);
  });

  it("fails a divergence with the message, field and text that differ", async () => {
    await assert.rejects(
      replayTest(client, "weather-7", async (replay) => {
        const answer = await chat(
          { baseURL: replay.openaiBaseUrl, apiKey: "traza", fetch },
          { model: "gpt-4o", messages: messages({ user: "What's the weather in Lyon?" }) },
        );
        if (answer.status !== 200) throw new Error(`the model call failed: ${answer.status}`);
      }),
      (error) => {
        assert.ok(error instanceof TapeMismatch);
        assert.match(error.message, /diverged from recorded run weather-7 at step 0/);
        assert.match(error.message, /message 1 \(user\): content differs at character 22/);
        assert.match(error.message, /got: {6}"What's the weather in Lyon\?"/);
        assert.match(error.cause.message, /409/, "the agent's own error is kept as the cause");
        return true;
      },
    );
  });

  it("fails an agent that stops early", async () => {
    await assert.rejects(
      replayTest(client, "weather-7", async (replay) => {
        await chat({ baseURL: replay.openaiBaseUrl, apiKey: "traza", fetch }, { model: "gpt-4o", messages: messages() });
      }),
      /stopped after 1 of the 2 model calls/,
    );
    assert.equal(describeReplay({ served: 2, remaining: 0, exhausted_calls: 0, diverged: null }), null);
    assert.match(describeReplay({ run: "r", served: 2, remaining: 0, exhausted_calls: 1, diverged: null }), /1 model call\(s\) past the end/);
  });
});

describe("golden runs", () => {
  it("are listed, marked and un-marked", async () => {
    let goldens = await client.goldens();
    assert.deepEqual(goldens.runs.map((run) => [run.run, run.source]), [["golden-run", "attribute"]]);
    await client.markGolden("weather-7");
    goldens = await client.goldens();
    assert.deepEqual(goldens.runs.map((run) => [run.run, run.source]), [["golden-run", "attribute"], ["weather-7", "annotation"]]);
    await client.markGolden("weather-7", { golden: false });
    goldens = await client.goldens();
    assert.deepEqual(goldens.runs.map((run) => run.run), ["golden-run"]);
  });
});
