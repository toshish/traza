#!/usr/bin/env python3
"""Renders one JSON-RPC reply from stdin, in the shape the demo wants.

Kept out of the shell script deliberately: inline `python3 -c` inside single
quotes cannot use double quotes without escaping, and the escaping is where
this kind of script goes wrong.
"""

import json
import re
import sys
import textwrap


def load():
    return json.load(sys.stdin)


def indent(text, prefix="  "):
    for line in text.splitlines():
        print(prefix + line if line.strip() else "")


def tool_text(message):
    """A tool result: its text block, plus a note if structured data rode along."""
    if "error" in message:
        print("  error: " + message["error"]["message"])
        return
    result = message["result"]
    indent(result["content"][0]["text"])
    if "structuredContent" in result:
        keys = ", ".join(result["structuredContent"].keys())
        print()
        print("  [structuredContent: " + keys + "]")


def initialize(message):
    result = message["result"]
    print("  protocol: " + result["protocolVersion"])
    print("  server:   " + result["serverInfo"]["name"] + " " + result["serverInfo"]["version"])
    print("  offers:   " + ", ".join(sorted(result["capabilities"])))
    print()
    print("  instructions (a host puts these in front of the model once):")
    for line in textwrap.wrap(result["instructions"], 66):
        print("    " + line)


def tools(message):
    for tool in message["result"]["tools"]:
        print("  {:<20} {}".format(tool["name"], tool["title"]))


def resources(message):
    for entry in message["result"]["resources"]:
        print("  {:<30} {}".format(entry["uri"], entry["title"]))


def templates(message):
    for entry in message["result"]["resourceTemplates"]:
        print("  {:<30} {}".format(entry["uriTemplate"], entry["title"]))


def prompts(message):
    for entry in message["result"]["prompts"]:
        print("  /{:<29} {}".format(entry["name"], entry["description"]))


def prompt(message):
    messages = message["result"]["messages"]
    indent(messages[0]["content"]["text"])
    attached = messages[1]["content"]["resource"]
    print()
    print(
        "  [attached: {}, {} bytes of live orientation]".format(
            attached["uri"], len(attached["text"])
        )
    )


def first_line(message):
    print("    " + message["result"]["content"][0]["text"].splitlines()[0])


def whole_text(message):
    indent(message["result"]["content"][0]["text"], "    ")


def rpc_error(message):
    print("    " + message["error"]["message"])


def trace_id(message):
    """The first trace id in a tool result — one tool's output is the next one's input."""
    found = re.search(r"trace=(\S+)", message["result"]["content"][0]["text"])
    print(found.group(1) if found else "")


def size(message):
    """How big the reply actually was, in the units the ceiling is counted in."""
    body = json.dumps(message["result"], separators=(",", ":"), ensure_ascii=False)
    print("    {} UTF-8 bytes on the wire".format(len(body.encode("utf-8"))))


MODES = {
    "tool": tool_text,
    "initialize": initialize,
    "tools": tools,
    "resources": resources,
    "templates": templates,
    "prompts": prompts,
    "prompt": prompt,
    "first-line": first_line,
    "whole-text": whole_text,
    "rpc-error": rpc_error,
    "trace-id": trace_id,
    "size": size,
}

if __name__ == "__main__":
    MODES[sys.argv[1]](load())
