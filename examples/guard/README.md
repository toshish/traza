Claude Code hook: a run fuse before every tool call
===================================================

[`claude-code-hook.sh`](claude-code-hook.sh) puts Traza's
[run fuses](../../docs/guide/guard.md) in front of Claude Code's tool calls.
The run is the Claude Code session: its id is the run id Traza checks.

- As a **PreToolUse** hook it is the pre-call gate. Before each tool call it
  asks `POST /v1/fuses/check` whether the session may take another step,
  naming the tool. On a `stop` verdict it exits 2 with the reasons on stderr:
  Claude Code blocks the call and shows the reasons to the model. A `warn`
  is printed and the call proceeds.
- As a **PostToolUse** hook (optional) it records each tool call that ran as
  a span — `session.id` set to the session, `gen_ai.operation.name:
  execute_tool`, the tool's name, and its input — so the session's step and
  repeat limits have something to count. When the acknowledgement says the
  run is now stopped it exits 2 at once, so the model hears it after the
  call that crossed the limit; that call has already run, and the next one
  is the one the gate refuses.

Without the PostToolUse half, a verdict covers whatever Traza already holds
under the session id — spans from any instrumentation that sets
`session.id` (or another [recognized session key](../../docs/llm-semantics.md))
to it — and the kill switch, which works with nothing recorded at all:

```sh
curl -X POST http://127.0.0.1:8080/v1/fuses/runs/$SESSION_ID/trip \
  -d '{"reason": "stop this session"}'
```

Install
-------

Requires POSIX `sh`, `curl` and `python3`. In `.claude/settings.json` (or
`~/.claude/settings.json` for every project):

```json
{
  "hooks": {
    "PreToolUse": [
      {"matcher": "*", "hooks": [{"type": "command", "command": "/path/to/claude-code-hook.sh"}]}
    ],
    "PostToolUse": [
      {"matcher": "*", "hooks": [{"type": "command", "command": "/path/to/claude-code-hook.sh"}]}
    ]
  }
}
```

Then give the session a policy. Out of the box the server's global policy
only **warns** on 12 repeats of the same call with the same input; to stop a
looping session instead, either start the server with
`--fuse-max-repeats 5 --fuse-action stop`, or create a policy for the
service the PostToolUse spans carry (`claude-code`):

```sh
curl -X POST http://127.0.0.1:8080/v1/fuses/policies -d '{
  "name": "claude-code", "match": {"service": "claude-code"},
  "limits": {"max_repeats": 5, "max_steps": 400}, "action": "stop"}'
```

A session stopped by a limit stays stopped until someone resets it:

```sh
curl -X POST http://127.0.0.1:8080/v1/fuses/runs/$SESSION_ID/reset \
  -d '{"grant": {"max_steps": 800}}'
```

Configuration
-------------

| Variable | Default | Meaning |
|---|---|---|
| `TRAZA_URL` | `http://127.0.0.1:8080` | The server. |
| `TRAZA_TOKEN` | none | Bearer token, when the server enforces auth. A `ro` token is enough for the gate; recording needs `rw`. It is passed to curl through a here-document, never on a command line. |
| `TRAZA_TENANT` | the default tenant | The run's tenant, for an unbound token. A bound token is forced to its own. |
| `TRAZA_RECORD_TOOL_INPUT` | off | `1` records each tool's full input on its span. By default only a SHA-256 of the canonical input is recorded — enough for repeat detection, without copying commands and file contents into the store. |
| `TRAZA_FUSE_FAIL_CLOSED` | off | `1` blocks the tool call when Traza cannot answer — unreachable, or an error instead of a verdict, which is what the gate gives for a run it could read only part of and that part does not stop ([why](../../docs/guide/guard.md#what-is-durable-and-what-is-rebuilt)). By default the hook fails open: it prints a warning and lets the call through. |
| `TRAZA_TIMEOUT` | `5` | Seconds to wait for Traza. |
| `CLAUDE_SESSION_ID` | the hook input's `session_id` | The run id. |

What it does not do
-------------------

- **The gate cannot see inside a step.** It answers before the call from
  what has been recorded; a single tool call that runs away on its own is
  not interrupted.
- **Recorded spans carry no duration.** A PostToolUse hook fires after the
  call, without its start time, so each span starts and ends at the moment
  it is recorded; `max_duration_seconds` then measures from the first
  recorded call to the last.
- **Cost and token limits need cost and token data.** The PostToolUse spans
  carry neither; those limits act only on spans from instrumentation that
  records model usage under the same session id.
- **Fail-open is the default.** A guard that blocks all work when the guard
  is down is a choice, and `TRAZA_FUSE_FAIL_CLOSED=1` is where to make it.

`tests/guard.rs` runs this script against a real server: a fresh session
passes, three identical recorded calls stop it under a repeat limit of
three, the gate then refuses, a reset lets it through, the kill switch
stops it again, and an unreachable server fails open (or closed, when
asked).
