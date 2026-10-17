#!/usr/bin/env python3
"""A stand-in model provider for the replay demo: the OpenAI chat-completions
wire format, answered by a script instead of a model.

The demo needs a model to record against, and must not need a key or the
network to do it. This is that model: it asks for `get_weather` when the last
message is the user's, and answers from the tool result when the last message
is the tool's. It is deterministic, so two runs with the same prompt get the
same answers — and a changed system prompt gets a changed answer, which is
what the demo's diff then finds.

    model.py PORT_FILE COUNT_FILE

It listens on an ephemeral port on 127.0.0.1, writes the port to PORT_FILE,
and appends one line to COUNT_FILE per completion it serves, so the demo can
count the model calls it answered even after it is stopped.
"""

import hashlib
import json
import os
import sys
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

MODEL = "sim-4o-2026-07"


def text_of(message):
    content = message.get("content")
    return content if isinstance(content, str) else ""


def complete(request):
    messages = request["messages"]
    system = " ".join(text_of(m) for m in messages if m.get("role") == "system")
    last = messages[-1]
    prompt_tokens = 12 + sum(len(json.dumps(m)) for m in messages) // 4
    if last.get("role") == "tool":
        weather = json.loads(last["content"])
        if "Fahrenheit" in system:
            degrees = "%d°F" % round(weather["celsius"] * 9 / 5 + 32)
        else:
            degrees = "%d°C" % weather["celsius"]
        answer = "It's %s in %s, %s. %s" % (
            weather["forecast"], weather["city"], degrees,
            "Take an umbrella." if weather["forecast"] == "raining" else "No umbrella needed.")
        message = {"role": "assistant", "content": answer}
        finish = "stop"
        completion_tokens = len(answer) // 4
    else:
        question = text_of(last)
        city = question.rsplit(" in ", 1)[-1].strip(" ?.") if " in " in question else "Paris"
        call_id = "call_" + hashlib.sha256((system + question).encode()).hexdigest()[:12]
        message = {"role": "assistant", "content": None, "tool_calls": [{
            "id": call_id, "type": "function",
            "function": {"name": "get_weather", "arguments": json.dumps({"city": city})}}]}
        finish = "tool_calls"
        completion_tokens = 17
    return {
        "id": "chatcmpl-standin-%d" % time.time_ns(),
        "object": "chat.completion",
        "created": int(time.time()),
        "model": MODEL,
        "choices": [{"index": 0, "message": message, "finish_reason": finish}],
        "usage": {"prompt_tokens": prompt_tokens, "completion_tokens": completion_tokens,
                  "total_tokens": prompt_tokens + completion_tokens},
    }


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        if not self.path.rstrip("/").endswith("/chat/completions"):
            self.send_error(404)
            return
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        reply = json.dumps(complete(body)).encode()
        with open(COUNT_FILE, "a") as handle:
            handle.write("1\n")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(reply)))
        self.end_headers()
        self.wfile.write(reply)

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    port_file, COUNT_FILE = sys.argv[1], sys.argv[2]
    server = HTTPServer(("127.0.0.1", 0), Handler)
    # Written to a temporary name and renamed, so the demo never reads half a port.
    with open(port_file + ".tmp", "w") as handle:
        handle.write(str(server.server_address[1]))
    os.replace(port_file + ".tmp", port_file)
    server.serve_forever()
