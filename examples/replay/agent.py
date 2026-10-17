#!/usr/bin/env python3
"""The replay demo's agent: a weather assistant on the OpenAI chat-completions
wire, written with nothing but the standard library.

    agent.py RESULT_FILE

Configured the way an OpenAI SDK is, by environment:

  OPENAI_BASE_URL   where chat completions go — a provider, or a Traza replay
  OPENAI_API_KEY    sent as `Authorization: Bearer`
  WEATHER_PROMPT    `v1` (default) or `v2`, the system prompt to use

and, to record the run into Traza the way OpenTelemetry GenAI
instrumentation would, TRAZA_URL, TRAZA_TOKEN and TRAZA_RUN (the session id).
Without TRAZA_RUN nothing is recorded.

The agent does not know or care whether OPENAI_BASE_URL is a model: that is
the point of the replay. It runs its real loop — call the model, run the tool
the model asks for, send the result back — and writes what it saw to
RESULT_FILE: every assistant message as received, every URL it sent a model
call to, and the final answer. Exit 0 on an answer, 4 when the model endpoint
refused a call (the refusal is in RESULT_FILE), 1 when either endpoint answered
with a redirect: one is never followed, so each key goes only to the URL it
was configured for.
"""

import hashlib
import json
import os
import sys
import time
import urllib.error
import urllib.request

PROMPTS = {
    "v1": "You are a weather assistant. Use the get_weather tool, then answer in one sentence.",
    "v2": "You are a weather assistant. Use the get_weather tool, then answer in one sentence. "
          "Always give temperatures in Fahrenheit.",
}
QUESTION = "What's the weather in Paris?"
TOOLS = [{"type": "function", "function": {
    "name": "get_weather",
    "description": "Current weather for a city.",
    "parameters": {"type": "object", "properties": {"city": {"type": "string"}},
                   "required": ["city"]}}}]


def get_weather(city):
    """The tool. It runs for real on every run, replays included — a replay
    answers model calls, not tool calls."""
    return {"city": city, "celsius": 12, "forecast": "raining"}


class RedirectRefused(Exception):
    """A 3xx answered a request that carried a key."""


class RefuseRedirects(urllib.request.HTTPRedirectHandler):
    """Refuses every redirect instead of following it.

    urllib copies a request's headers onto the redirected one, Authorization
    included: to another host, and from https to plain http. Each key belongs
    to the one endpoint it was configured for (the model, or Traza's ingest),
    so a redirect is refused whatever sent it, and the key follows it nowhere.
    """

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise RedirectRefused(
            "%s answered %d, a redirect to %s: not followed, so the key goes only "
            "to the URL it was configured for" % (req.full_url, code, newurl))


# Built once: building an opener reads the proxy settings, which on macOS
# asks the system configuration.
OPENER = urllib.request.build_opener(RefuseRedirects)


def post(url, key, body):
    req = urllib.request.Request(url, data=json.dumps(body).encode(), method="POST")
    req.add_header("Content-Type", "application/json")
    req.add_header("Authorization", "Bearer " + key)
    with OPENER.open(req, timeout=10) as reply:
        return json.loads(reply.read())


# --------------------------------------------- the instrumentation, by hand


def parts(message):
    """An OpenAI message in OTel GenAI's `parts` shape, as instrumentation
    records it. The replay compares by what was said, not by how it was
    spelled, so recording in one shape and replaying in another is fine."""
    out = []
    if message.get("content"):
        out.append({"type": "text", "content": message["content"]})
    for call in message.get("tool_calls") or []:
        out.append({"type": "tool_call", "id": call["id"], "name": call["function"]["name"],
                    "arguments": json.loads(call["function"]["arguments"])})
    if message["role"] == "tool":
        out = [{"type": "tool_call_response", "id": message["tool_call_id"],
                "response": json.loads(message["content"])}]
    return {"role": message["role"], "parts": out}


class Recorder:
    def __init__(self):
        self.run = os.environ.get("TRAZA_RUN")
        self.spans = []
        if self.run:
            self.trace = hashlib.sha256((self.run + str(time.time_ns())).encode()).hexdigest()[:32]
            self.root = "root-" + self.trace[:8]

    def span(self, name, start, attributes, status="ok"):
        if not self.run:
            return
        attributes = dict(attributes, **{"session.id": self.run})
        self.spans.append({
            "trace_id": self.trace, "span_id": "%s-%02d" % (self.trace[:8], len(self.spans) + 1),
            "parent_span_id": self.root, "name": name, "service": "weather-agent",
            "status": status, "start_time_ns": start, "end_time_ns": time.time_ns(),
            "attributes": attributes})

    def flush(self, start):
        """One batch at the end, as a batch exporter flushing on shutdown."""
        if not self.run:
            return
        root = {"trace_id": self.trace, "span_id": self.root, "name": "weather-agent run",
                "service": "weather-agent", "status": "ok", "start_time_ns": start,
                "end_time_ns": time.time_ns(), "attributes": {"session.id": self.run}}
        ack = post(os.environ["TRAZA_URL"] + "/v1/spans", os.environ["TRAZA_TOKEN"],
                   [root] + self.spans)
        if ack.get("accepted") != len(self.spans) + 1:
            sys.exit("not every span was stored: %s" % ack)


# -------------------------------------------------------------- the loop


def main(result_path):
    base = os.environ["OPENAI_BASE_URL"].rstrip("/")
    key = os.environ["OPENAI_API_KEY"]
    model_request = "sim-4o"
    messages = [
        {"role": "system", "content": PROMPTS[os.environ.get("WEATHER_PROMPT", "v1")]},
        {"role": "user", "content": QUESTION},
    ]
    result = {"responses": [], "model_calls": [], "answer": None, "refusal": None}
    recorder = Recorder()
    started = time.time_ns()
    for _ in range(4):
        url = base + "/chat/completions"
        result["model_calls"].append(url)
        start = time.time_ns()
        try:
            reply = post(url, key, {"model": model_request, "messages": messages, "tools": TOOLS})
        except urllib.error.HTTPError as error:
            result["refusal"] = {"status": error.code,
                                 "x-should-retry": error.headers.get("x-should-retry"),
                                 "body": json.loads(error.read() or b"{}")}
            break
        choice = reply["choices"][0]
        message = choice["message"]
        assistant = {"role": "assistant", "content": message.get("content")}
        if message.get("tool_calls"):
            assistant["tool_calls"] = message["tool_calls"]
        result["responses"].append({
            "content": message.get("content"),
            "tool_calls": [{"id": c["id"], "name": c["function"]["name"],
                            "arguments": json.loads(c["function"]["arguments"])}
                           for c in message.get("tool_calls") or []],
            "finish_reason": choice["finish_reason"],
        })
        recorder.span("chat " + model_request, start, {
            "gen_ai.operation.name": "chat",
            "gen_ai.request.model": model_request,
            "gen_ai.response.model": reply["model"],
            "gen_ai.usage.input_tokens": reply["usage"]["prompt_tokens"],
            "gen_ai.usage.output_tokens": reply["usage"]["completion_tokens"],
            "gen_ai.input.messages": json.dumps([parts(m) for m in messages]),
            "gen_ai.output.messages": json.dumps(
                [dict(parts(assistant), finish_reason=choice["finish_reason"])]),
        })
        messages.append(assistant)
        if not message.get("tool_calls"):
            result["answer"] = message["content"]
            break
        for call in message["tool_calls"]:
            start = time.time_ns()
            args = json.loads(call["function"]["arguments"])
            weather = get_weather(**args)
            recorder.span("execute_tool get_weather", start, {
                "gen_ai.operation.name": "execute_tool",
                "gen_ai.tool.name": call["function"]["name"],
                "gen_ai.tool.call.id": call["id"],
                "gen_ai.tool.call.arguments": json.dumps(args),
                "gen_ai.tool.call.result": json.dumps(weather),
            })
            messages.append({"role": "tool", "tool_call_id": call["id"],
                             "content": json.dumps(weather)})
    recorder.flush(started)
    with open(result_path, "w") as handle:
        json.dump(result, handle)
    return 0 if result["answer"] is not None else 4


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1]))
    except RedirectRefused as refusal:
        sys.exit("REFUSED: %s" % refusal)
