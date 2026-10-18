# Recording coding agents: Claude Code and Codex, zero setup

Claude Code and the Codex CLI already write down everything they do: one JSONL
transcript per session, on your own disk. `traza-server import` turns those
transcripts into runs — every session a run, every model response an llm step,
every tool call a tool step, every sub-agent an agent span — with no SDK, no
OpenTelemetry exporter, no proxy and no change to how you use either tool.
Point it at the default locations and every session the machine has ever run
becomes a run Traza can [tape, replay, diff and recall](replay.md),
[guard](guard.md), and **blame**: ask which agent step, under which prompt and
which model, wrote a given line of your code.

```bash
traza-server --data-dir ./data &              # a server (or use one you have)
traza-server import claude-code               # every Claude Code session here
traza-server import codex                     # every Codex session here
traza-server import claude-code --watch &     # and keep following live ones
traza-server blame src/cart.rs:42             # who wrote line 42?
```

## What it reads

| Source | Default `--path` | Files |
|---|---|---|
| `claude-code` | `~/.claude/projects` | `<project>/<sessionId>.jsonl`, and each sub-agent's `<sessionId>/subagents/[workflows/<wf>/]agent-<id>.jsonl` with its `.meta.json` |
| `codex` | `~/.codex/sessions` | `YYYY/MM/DD/rollout-<time>-<thread>.jsonl`, and the `.jsonl.zst` Codex compresses a finished rollout into |

The formats are undocumented and change with their tools, so they are read
leniently: a record of a type the importer does not know is **counted under
its type and skipped**, a line that is not JSON is counted as malformed, and
neither stops the import. The shapes that are understood — observed on real
installations of Claude Code 2.x and Codex CLI 0.100–0.160 — are written down
in the [`traza::import` module docs](../../src/import/mod.rs); the test
fixtures in `tests/fixtures/import/` are synthetic transcripts in those shapes.

Compressed Codex rollouts are decompressed by Traza's own Zstandard decoder
(RFC 8878, no dependency, like its gzip inflate): the content checksum is
verified when the frame carries one, a decompressed file is capped at 2 GiB,
and dictionaries — which Codex does not use — are refused.

## What becomes what

One trace per session — sub-agents included — so every parent link can be
derived from data both sides of it hold.

| Transcript | Span | Notable attributes |
|---|---|---|
| a session | the run: every span carries `session.id` (Claude Code's `sessionId`; for Codex, the root thread id) | `traza.source`, `code.project` (the session's working directory), `service.version` (the tool's version), `vcs.ref.head.name`; `service` is `claude-code` or `codex` |
| a user prompt | a `turn` span (not a step) | event `gen_ai.user.message` with the prompt as `content`; the session's first prompt, cut to 200 characters, is also `session.goal`; status is that of the turn's last model call |
| a model response | an llm step, child of its turn | `gen_ai.operation.name=chat`, `gen_ai.provider.name` (`anthropic`, `openai`), `gen_ai.request.model`, `gen_ai.usage.input_tokens` / `output_tokens` / `cache_read.input_tokens` / `cache_creation.input_tokens`, `gen_ai.input.messages` (the conversation since the previous response), `gen_ai.output.messages` (text, reasoning, tool calls) |
| a tool call and its result | a tool step, child of the response that asked for it | `gen_ai.tool.name`, `gen_ai.tool.call.id`, `gen_ai.tool.call.arguments` (kept as JSON), `gen_ai.tool.call.result`; status `error` when Claude Code marks the result an error or a Codex command exits non-zero (`process.exit.code`) |
| a sub-agent | an agent step, `gen_ai.operation.name=invoke_agent` | `gen_ai.agent.name` (Claude Code's `agentType`, Codex's `agent_role`); a Claude Code sub-agent is a child of the `Agent`/`Task` tool call that spawned it and its own calls are its children |
| an API error Claude Code synthesized | a failed llm step | `error.type`; the model is the one the session was using |
| a Codex token count no recorded response claims | an llm step with its usage and `codex.content_unrecorded=true` | the call happened and was paid for; the rollout kept no content for it |

**Codex turns recorded as items.** Newer Codex builds record some turns only
as `event_msg/item_completed` events (`Reasoning`, `AgentMessage`,
`CommandExecution`, `FileChange`, `McpToolCall`, `WebSearch`, …) instead of
response items. Every turn's items are held until the turn ends: when the turn
also recorded response items the held items are duplicates and are dropped;
when it did not, they are replayed in order as the turn's responses (split by
their token counts) and tool calls (a tool item belongs to the response that
asked for it, and its result is the next response's input). Such a turn
therefore appears when it ends, not while it runs. An item that names a turn
already settled is counted as `item_completed/late` and dropped.

Two token conventions ride side by side, and the provider says which: Anthropic
reports `input_tokens` as the *uncached remainder* with the cache counters
beside it, OpenAI reports it *including* the cached part. Claude Code writes one
API response as several records repeating the same usage; it is counted once
per response, never per record. **No cost is invented**: tokens are recorded
and the server's [`--pricing`](../configuration.md#model-pricing) table prices
them like any other span's.

Text is bounded so an import cannot balloon: a tool result, argument string or
prompt past 1 MiB, and a message part past 64 KiB, is cut with a marker naming
how many bytes were dropped; image data is replaced by a placeholder. The
server's payload offload then applies as usual.

## Importing again is safe

Every id is derived from the transcript's own identity — the session, plus a
message id, tool-use id, record uuid, agent id, Codex call id or line number —
through SHA-256. Converting the same lines produces the same spans byte for
byte, and the store's primary key is last-write-wins, so importing twice
leaves one copy of each span. Ids do not depend on file paths, so they survive
moving `~/.claude` to another machine.

## Live: `--watch` and the state file

`--watch` keeps polling (`--interval-ms`, default 1000) for appended lines and
new files and sends what changed, so a session you are running shows up as a
live run within about a second — which is what lets the [run fuses](guard.md)
judge a Claude Code session while it runs. A line still being written (no
newline yet, not yet valid JSON) waits for the next poll.

Progress is kept in a state file — `~/.traza/import-state.json`, or
`--state FILE` — per destination (a server URL and tenant, or a data
directory), so importing into a scratch directory does not make the next import
into your server skip anything. A file is re-read from its start when it grows
(the current turn lives in earlier lines), and only spans touched by a line past
the recorded offset are sent; a span built entirely from earlier lines was
already sent in exactly its final form, so a restart sends nothing twice.
`--watch` keeps the converters of active files in memory instead (at most 64,
each dropped after ten idle minutes), so following a live file reads only its
new lines. When Codex replaces a finished rollout with its `.jsonl.zst`, the
compressed file continues the plain file's progress (after checking it starts
with the same bytes). `--no-state` sends everything every time.

The state file is written atomically after each change; a crash at worst
re-sends spans the destination already holds, which is harmless.

## Destinations, tenants, credentials

- `--url URL` (default `http://127.0.0.1:8080`) with `--token` or `TRAZA_TOKEN`:
  `POST /v1/spans` to a running server, which needs `rw`. A credential bound to
  a tenant stamps that tenant on every span; `--tenant` names one for an
  unbound credential. The client speaks plain HTTP and connects only to the URL
  given.
- `--data-dir DIR`: straight into a data directory no server holds (the
  directory's lock refuses one a server is running on), with the server's
  default payload threshold.
- `--dry-run`: convert and count, send nothing, record nothing.

A batch the destination refuses is split until the refused spans are isolated;
they are counted and named, and the rest still land. A refused credential, an
unreachable server or an exhausted quota stops the import with the server's own
words. The exit code is 0, or 3 when some spans or files were refused.

`--since 7d` only reads files modified that recently (a selected session is
imported whole); `--project-filter SUBSTR` only sessions whose file path or
working directory contains it. `--json` prints the counts as JSON:

```json
{"source":"claude-code","files_seen":3,"files_read":3,"files_unchanged":0,
 "files_filtered":0,"files_failed":0,"sessions":2,"spans_built":21,
 "spans_sent":21,"spans_refused":0,"turns":3,"llm_steps":11,"tool_steps":6,
 "tool_errors":1,"agent_spans":1,"input_tokens":240,"output_tokens":625,
 "cache_read_tokens":8400,"cache_creation_tokens":3650,
 "transcript":{"lines":28,"malformed_lines":1,"records":{"assistant":13,
 "summary":1,"system/api_error":1,"system/stop_hook_summary":1,
 "user/prompt":4,"user/tool_result":6},
 "unknown_records":{"x-future-record":1},"unknown_blocks":0,"capped":0},
 "errors":[]}
```

All flags are in [configuration](../configuration.md#traza-server-import-and-traza-server-blame).

## Agent Blame

`git blame` says which commit last touched a line. When an agent wrote the
code, the commit is the least informative fact about it. Traza holds every edit
the agent made, so it can answer the question a reviewer actually has: which
session, which step, under which prompt, with which model introduced this exact
line — and open the run around it.

```text
$ traza-server blame src/cart.rs:4
src/cart.rs:4  self.items.iter().map(|item| item.price).sum()
  introduced 2026-01-05T10:00:06Z by Edit in run 11111111-1111-4111-8111-111111111111  [claude-code, claude-sonnet-4-5-20250929, step 3]
    prompt: Add a total to the cart summary
    file /tmp/demo-shop/src/cart.rs  trace 257eaef69d566d3579947aac638045ca  span 9694fd7a902a860f
```

The same answer is [`GET /v1/blame`](http-api.md#get-v1blame) and the
[`blame_line`](mcp.md#the-tools) MCP tool, so an agent can ask why code looks
the way it does before it changes it.

**What counts as introducing a line.** A tool step introduces a line into a
file when its edit adds the line more times than it removes it, compared with
whitespace normalised (runs of whitespace fold to one space, ends trimmed — so
re-indenting is not authorship, as with `git blame -w`). Edits are read from
the arguments by shape, not by one vendor's tool names: `new_string`/
`old_string` (Claude Code `Edit`), `edits[]` (`MultiEdit`), `content` from a
tool whose name contains `write` (`Write`: every line counts), `new_source`
(`NotebookEdit`), the `create`/`str_replace`/`insert` commands of Anthropic's
text-editor tool, and any Codex patch (`*** Begin Patch`) anywhere in the
arguments — `+` lines added and `-` lines removed per file, relative paths
joined with the working directory the Codex importer records. It therefore
works on Claude Code and Codex sessions recorded any way, not only imported
ones, as long as the arguments were recorded.

**Which file.** The query path matches when it is a suffix of the recorded
path at a component boundary (`src/lib.rs` matches `/repo/src/lib.rs`, not
`/repo/src/glib.rs`; a leading `/` is not an anchor). The CLI sends the path
relative to the repository root (the nearest directory holding `.git`), so a
line written in a worktree or another checkout of the same repository is still
found.

**Which answer.** The earliest introduction first, then later ones (a line
introduced, deleted and re-introduced has several), up to `limit` (default 5,
at most 50). Each names its run, tenant, trace and span, the tool, the file as
recorded, when, the prompt that started that turn (or the task a sub-agent was
given, with the sub-agent named), the model whose response asked for the edit,
and the step's position on the run's tape. `traza-server blame` exits 0 when it
found one and 1 when no recorded edit introduced the line.

**How it is found.** Candidates come from the content index: a span must
contain the file name's words and the line's words — only the line's words
whose left neighbour is printable ASCII punctuation or a space, because those
are the only words a JSON-escaped copy of the line (arguments recorded as a
string, where a newline becomes `\n`) tokenises the same way. Every candidate is
verified by parsing its edit, so the index only makes the search faster. The
search and the reading of each match's run share one query deadline; at most
200,000 candidates are verified (`truncated` says when more existed); a line
with no searchable word at all, in the line or the file name, is refused.

## Limits, honestly

- **Formats.** Both formats are undocumented; a change the importer does not
  understand shows up as `unknown_records` / `unknown_blocks` in the counts, not
  as an error. Claude Code builds that wrote sub-agent records *inline* in the
  main transcript (`isSidechain: true` there) are counted as
  `sidechain-inline` and skipped — without the sub-agent's own file they cannot
  be placed.
- **Codex sub-agents** join their root thread's run as agent spans but are not
  nested under the `spawn_agent` call that created them: the rollout does not
  record which call spawned which thread. A sub-agent whose parent rollout is
  not on disk becomes a run of its own; if the parent appears later its spans
  are sent again under the root's ids, and the earlier copy remains until
  erased. Claude Code workflow agents (which name no tool call) are agent spans
  at the top of the session's trace.
- **Codex content.** A model call whose token count survives but whose items
  do not is kept for its usage, marked `codex.content_unrecorded`. A
  plain-text rollout whose last turn never ended keeps that turn's held items
  unconverted until it ends (a compressed rollout is final, so its last turn is
  settled when it is read).
- **Rewrites.** Transcripts only ever grow, so a file is re-examined only when
  its size moves (a stat per file per poll); one rewritten in place to exactly
  its old size is not noticed.
- **Prices.** Pricing applies the input and output rates; Anthropic cache reads
  and writes are recorded but not priced separately, so a cache-heavy session's
  derived cost understates it.
- **Counting.** A session rollup's `llm_calls` counts every span carrying
  `gen_ai.operation.name`, tool and agent spans included (Traza's existing rule
  for GenAI spans); a [tape](replay.md) counts steps exactly.
- **Fuses.** Imported spans pass through the [run fuses](guard.md) like any
  ingest. A historical import fills the guard's bounded live state and may warn
  on old runs; a live one is the point.
- **Size.** One trace per session means a very long session is a very large
  trace. Each file is capped at 200,000 spans (`capped` counts what was left
  out), a line at 64 MiB, a decompressed rollout at 2 GiB.
- **Blame** cannot see lines written by shell commands (`sed -i`, heredocs,
  generators), edits whose arguments were not recorded, or the part of an
  argument offloaded to the payload store beyond its inline preview.
