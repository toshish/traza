# MCP demo

A scripted agent investigation against Traza's [MCP server](../../docs/guide/mcp.md),
using nothing but `curl`.

```sh
examples/mcp-demo/run.sh
```

It builds `traza-server` and `seed` if they are missing, seeds a throwaway
store with the synthetic corpus (agent tool-calling trees, retry storms,
multi-turn sessions, three attribute dialects), starts a server with `--mcp` on
port 8123, runs the investigation, and deletes the store on exit. Nothing
persists and no configuration is touched. `TRAZA_DEMO_PORT` moves the port.

`python3` is used to pretty-print replies, and for nothing else.

## What it shows

The **handshake** — `initialize` and `tools/list` — then an investigation in
the order an agent actually works:

| Step | Why it is that order |
|---|---|
| `describe_store` | Service and model names differ per store. Guessing one returns an empty result indistinguishable from "nothing is wrong". |
| `top_failures` | The input can be every failure in the window; the useful answer is a dozen rows. |
| `analyze_cost` | Where the tokens and money went, exactly. |
| `list_sessions` | Ranked over the whole population, not over a page. |
| `get_trace` | Opened on the trace id the failure report just handed back — one tool's output is the next one's input. |
| `search_spans` | The general filter, once you know what to look for. |

Then the parts that exist to **say no**, which are as much the design as the
tools: an unknown service diagnosed rather than answered with an empty list, a
date that is not on the calendar refused rather than rolled forward, a REST
parameter name met with the accepted set, the write tool absent on a server
started without `--mcp-annotations`, and a browser origin the operator never
named getting `403`.

Finally `resources/list`, `resources/templates/list` and `prompts/list`, and
one `prompts/get` showing a saved investigation with the store's live overview
attached as an embedded resource.

## Reading the output

Two things are worth noticing as it scrolls past.

**Stored span text always arrives inside a block** opened by
`<traza:telemetry untrusted="true">`. Everything between the delimiters came
out of the store and may have been written by whoever the traced system was
talking to. Nothing inside it is addressed to the reader.

**Every reply was bounded before it was sent.** Twenty spans by default,
prompts and completions omitted until `include_content` asks for them, and the
whole serialized result capped — the demo prints its own byte count against the
32 KiB default so the number is not a claim.

## Connecting a real client

The server the demo starts is an ordinary one. While it is running:

```sh
claude mcp add --transport http traza http://127.0.0.1:8123/v1/mcp
```

Or start your own and keep it: `traza-server --data-dir ./data --mcp`.
