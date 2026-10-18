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
verified when the frame carries one, the compressed file is read within
2 GiB and its content capped at 2 GiB (a longer one is refused and counted
in `files_failed`), and dictionaries — which Codex does not use — are
refused.

## What becomes what

One trace per session — sub-agents included — so every parent link can be
derived from data both sides of it hold.

| Transcript | Span | Notable attributes |
|---|---|---|
| a session | the run: every span carries `session.id` (Claude Code's `sessionId`; for Codex, the root thread id) | `traza.source`, `code.project` (the session's working directory), `service.version` (the tool's version), `vcs.ref.head.name`; `service` is `claude-code` or `codex` |
| a user prompt | a `turn` span (not a step) | event `gen_ai.user.message` with the prompt as `content`; the session's first prompt, cut to 200 characters, is also `session.goal`; status is that of the turn's last model call |
| a model response | an llm step, child of its turn | `gen_ai.operation.name=chat`, `gen_ai.provider.name` (`anthropic`, `openai`), `gen_ai.request.model`, `gen_ai.usage.input_tokens` / `output_tokens` / `cache_read.input_tokens` / `cache_creation.input_tokens`, `gen_ai.input.messages` (the conversation since the previous response), `gen_ai.output.messages` (text, reasoning, tool calls), `gen_ai.conversation.id` (the session; `<session>/<agent id>` for a Claude Code sub-agent; the thread for Codex) and `traza.input.delta` (`true`: the input is what was added since the conversation's previous call; `false` on the first call after a compaction, whose input — the summary and what followed, or Codex's replacement history — is all it saw) |
| a tool call and its result | a tool step, child of the response that asked for it | `gen_ai.tool.name`, `gen_ai.tool.call.id`, `gen_ai.tool.call.arguments` (kept as JSON), `gen_ai.tool.call.result`; status `error` when Claude Code marks the result an error or a Codex command exits non-zero (`process.exit.code`), `ok` once a result says otherwise, and none while no result was recorded (a call that never returned claims no outcome); `traza.edit=true` on a Codex call that applies a patch through a general tool (an older build's `shell ["apply_patch", …]`), so blame can find it without reading every command |
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
server's payload offload then applies as usual — but it moves long *string*
values out of the record, not structured ones, so a span's structured
attributes (the input and output messages, JSON arguments) are also held to
1 MiB each and 2 MiB together, serialized: past that, the later parts, items
or fields are replaced by a marker saying how many were not kept, and the span
is counted in `spans_trimmed`. Without it, a response of hundreds of large
parts overran a server's 4 MiB `max-span-bytes` cap and was refused whole (on
one real Codex history, one span of 894,000).

## What the recorded runs answer

An imported session is a run like any other, so every read built on runs
works on it with nothing else set up. On the synthetic fixtures in
`tests/fixtures/import/` (`tests/cross_feature.rs` holds these assertions):

- **[Foresight](foresight.md).** The sessions of one tool are one cohort —
  agent `claude-code` or `codex`, a sub-agent's steps part of its session's
  path — so
  `GET /v1/fate/map?agent=claude-code` shows which first moves of past
  sessions ended well, and `GET /v1/fate/runs/{session}` reads a live
  session against them.
- **[Context X-Ray and Cache Autopsy](xray.md).** Each call's context is
  rebuilt from the conversation's recorded deltas: an X-Ray shows the whole
  conversation the model saw, with each tool result traced to the tool step
  that returned it, and the autopsy pairs each call with its conversation's
  previous one — a break after a long pause reads `ttl_expired`, priced when
  the pricing table names cache rates.
- **[TQL and the detectors](tql.md).** Tool names, arguments and error
  statuses are the steps' own, so `test[error] ..> edit` and the
  `unverified_finish` detector find the session that edited code after a
  failing test and stopped without running it again.
- **[Agent Blame](#agent-blame)**, below, and **[run topology](steer.md#topology)**:
  a Claude Code sub-agent is a handoff from `claude-code` and back.
- **[The change radar](radar.md)** reads `service.version`, so a tool upgrade
  is a change point.
- **[Live runs and the hook pack](steer.md).** A session imported with
  `--watch` is live while it runs; with the
  [Claude Code hook pack](../../examples/claude-code/README.md) installed, its
  run is the same session id, so the pre-call gate, nudges and foresight all
  apply to it.

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
already sent in exactly its final form, so a restart sends nothing twice. A
file counts as unchanged while its size **and** its stamp — modification time,
and on Unix its device and inode — are what was recorded, so an idle tree costs
a stat per file; one that shrank, whose first bytes changed, or that was
written to without growing (rewritten in place, or replaced, at its old size)
is read again from its start and sent whole. A Claude Code sub-agent's
`.meta.json` is recorded the same way (its size and stamp, or that it is
missing): when it changes, the sub-agent's transcript is converted again and
its spans sent again — after a restart too, and for a transcript whose
converter was let go, though the transcript itself did not change.
`--watch` keeps the converters of live files in memory instead, so following
a live file reads only its new lines: only a file written to in the last ten
minutes keeps one, and never more than 64 at any moment — the first pass over a
whole history included, which therefore holds one file's converter at a time
plus the live ones; past the cap, the file modified longest ago gives its
converter up (it is rebuilt, with resend suppression, if that file grows
again). When Codex replaces a finished rollout with its `.jsonl.zst`, the
compressed file continues the plain file's progress (after checking it starts
with the same bytes). `--no-state` sends everything every time.

The state file's destination for `--url` is the server plus the tenant the
spans land in — a bound credential's own (asked of
[`GET /v1/whoami`](http-api.md#get-v1whoami) before anything is read), else
`--tenant` — so two credentials bound to different tenants of one server keep
separate progress, and rotating a credential keeps it. The token is never
written down.

A watch outlives its server: when the server cannot be reached, or keeps
answering `5xx` or `429` through the client's own retries (about 15 s), the
file being sent keeps its recorded offset, the pass stops, and the watch tries
again after a wait that doubles from the poll interval up to 30 s; what was
sent before the outage may be sent again, which is harmless. A watch started
before its server waits for it too. A refused credential still ends it.

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

Before reading anything, an import over `--url` asks the server who its
credential is: a refused credential, a read-only one, or a `--tenant` other
than a bound credential's own stops it there, with nothing sent. A batch the
destination refuses is split until the refused spans are isolated; they are
counted and named, and the rest still land — except a refusal that is about the
credential rather than a span (a tenant the credential is not bound to), which
stops the import instead of being split into a request per span. A refused
credential, and — for a batch import — an unreachable server or an exhausted
quota, stop the import with the server's own words. The exit code is 0, 3 when
some spans or files were refused, and 2 when the import could not run.

`--since 7d` only reads sessions with a file modified that recently, and reads
such a session whole: a Claude Code transcript with every sub-agent file of its
session, a Codex rollout with the rollouts of its parent threads (as far as the
rollouts on disk tell) — so an `Agent` call keeps its agent and a sub-agent
thread its root. `--project-filter SUBSTR` only reads sessions whose file path
or working directory contains it. `--json` prints the counts as JSON:

```json
{"source":"claude-code","files_seen":3,"files_read":3,"files_unchanged":0,
 "files_filtered":0,"files_failed":0,"sessions":2,"spans_built":21,
 "spans_sent":21,"spans_refused":0,"spans_trimmed":0,"turns":3,"llm_steps":11,
 "tool_steps":6,
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
re-indenting is not authorship, as with `git blame -w`). The edit is read from
the call's arguments by shape: `new_string`/`old_string` (Claude Code `Edit`),
`edits[]` (`MultiEdit`), `content` from a tool whose name contains `write`
(`Write`: every line counts), `new_source` (`NotebookEdit`), the
`create`/`str_replace`/`insert` commands of Anthropic's text-editor tool,
Codex file-change items (`changes` with a file's content or unified diff), and
any Codex patch (`*** Begin Patch`) anywhere in the arguments — `+` lines added
and `-` lines removed per file, relative paths joined with the working
directory the Codex importer records. It therefore works on Claude Code and
Codex sessions recorded any way, not only imported ones, as long as the
arguments were recorded.

**A failed edit introduces nothing.** What became of an edit is the step's own
record — its `outcome`. A step whose status is `error` **failed**: Claude Code's
`old_string` was not found, a Codex patch did not apply, the tool was refused.
These tools are all-or-nothing, so a failed step wrote nothing and is never an
answer, however many times it asked to add the line — a failed attempt followed
by a successful one is blamed on the successful one. A step with status `ok`, or
with a recorded result, is **applied**. A step with neither — no status, no
result: a call still running, or one whose result was never written (the
importers leave a call's status unset until its result arrives) — is
**unconfirmed**: it may not have happened, so it is answered after every applied
introduction, marked as such.

**Which file, in which repository.** On its own, the query path matches when
it is a suffix of the recorded path at a component boundary (`src/lib.rs`
matches `/repo/src/lib.rs`, not `/repo/src/glib.rs`; a leading `/` is not an
anchor) — in every project the store holds, so a common line in a common path
(`}` in `src/main.rs`) can be answered from an unrelated repository. Scoped to
one or more `project`s (absolute repository roots), a relative path must name
exactly `<project>/<path>`; a project written `<dir>/*` stands for every
checkout directly under `<dir>`. `traza-server blame` scopes itself: it sends
the path relative to the repository root (the nearest directory holding
`.git`) with that root, the main checkout, every linked worktree git lists for
the repository (read from git's own files), and the directories of checkouts
nested inside it — `<repo>/.claude/worktrees/*`, where Claude Code puts the
worktrees its agents work in and removes them afterwards, when git forgets
them. So a line written in a worktree is found, a deleted one included, and
one written into some other repository's `src/main.rs` is not.
`--anywhere` lifts the scope (for sessions recorded at another path or on
another machine) and each answer then names its `project`. The
[`blame_line`](mcp.md#the-tools) tool takes the same `project`.

**Which answer.** The earliest applied introduction first, then later ones (a
line introduced, deleted and re-introduced has several), then any unconfirmed
ones, earliest first, up to `limit` (default 5, at most 50). Each names its
`outcome` (`applied` or `unconfirmed`), run, tenant, trace and span, the tool,
the file as recorded, when, the prompt that started that turn (or the task a
sub-agent was given, with the sub-agent named), the model whose response asked
for the edit, and the step's position on the run's tape; the CLI prints an
unconfirmed one as `attempted`, saying no result was recorded. `traza-server blame` exits 0 when it
found one, 1 when no recorded edit introduced the line, and 2 when it could not
ask.

**How it is found.** Candidates are the calls of the editing tools —
`Edit`, `MultiEdit`, `Write`, `NotebookEdit`, `apply_patch`,
`str_replace_based_edit_tool`, `str_replace_editor`, `text_editor`,
`edit_file`, `write_file`, `create_file` — found through the exact attribute
index on `gen_ai.tool.name` and `tool.name`, plus the tool steps the Codex
importer marked `traza.edit=true` (a shell call that applies a patch), so the
search decodes edit calls and nothing else, and every candidate is verified by
parsing its edit (a failed step is passed over before its edit is parsed). General-purpose shells are not candidates by name: in an
older Codex history nearly every call is a `shell` call. (The content index
cannot serve here either: a store of transcripts is mostly model calls carrying
whole conversations, whose word filters admit nearly every block for ordinary
code words.) The search and the reading of each match's run share one query
deadline; at most 200,000 candidates are verified, and the search stops
decoding the moment the cap is passed, answering `truncated: true` rather than
running on to the deadline.

**Measured** on one developer's real history (release build, Apple-silicon
laptop, each figure a whole `traza-server blame` process):

| Store | Spans | Edit candidates | A line no agent wrote | Lines with a recorded introduction |
|---|---|---|---|---|
| Claude Code history | 77,002 | 7,804 | 0.33 s | 76 of 100 (median 0.44 s, slowest 1.3 s) |
| plus the Codex history (2,216 rollouts) | 970,972 | 52,600 | 3.2 s | 26 of 30 (median 2.9 s, slowest 5.1 s) |

The lines were drawn at random from this repository's `src/` (lines with at
least 12 non-space characters), and the search was the CLI's default, scoped
to the repository; unscoped, it found the same lines. "A recorded
introduction" means blame found an edit in this repository that added the
line; whether that edit is what put today's copy of the line there was not
checked, and for a line written, deleted and written again the earliest is
reported. The Codex history holds 447,767 tool steps; with general-purpose
shells no longer candidates by name, 44,796 of them are. These figures were
taken before failed edits stopped counting as introductions: a line whose only
recorded "introductions" were failed attempts is now answered with none, and
one whose earliest attempt failed is now answered with the edit that applied,
so the counts in the last column are an upper bound until measured again.

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
  not on disk joins the run named by its parent's thread id (the parent's run,
  once that rollout appears, unless it has a parent of its own); only when the
  parent turns out to have a parent of its own does the run id change, and then
  the sub-agent's spans are sent again under the new ids and the earlier copy
  remains until erased. Claude Code workflow agents (which name no tool call)
  are agent spans at the top of the session's trace. A rollout found while its
  first (meta) line is still being written is not read until that line is
  whole — every id derives from the thread it names — and is asked again on
  the next poll; it has no complete line to convert in the meantime.
- **Late sub-agent metadata.** Claude Code may write a sub-agent's
  `.meta.json` after its transcript's first lines. The meta file's stamp (or
  its absence) is recorded with the transcript's progress, so whenever it
  appears or changes — during a watch, after a restart, or for a transcript no
  converter is kept for — the transcript is converted again: the agent span is
  re-parented under the `Agent` call, its spans renamed after the agent type,
  and the sub-agent's spans sent again (a stat of the meta file per sub-agent
  transcript per poll). A transcript whose meta file never appeared keeps its
  agent span at the top of the trace. A meta file over 64 KiB (real ones are a
  few hundred bytes) is not read: it is named in `errors` and counted in
  `files_failed`, and the transcript is imported unplaced.
- **Codex content.** A model call whose token count survives but whose items
  do not is kept for its usage, marked `codex.content_unrecorded`. A
  plain-text rollout whose last turn never ended keeps that turn's held items
  unconverted until it ends (a compressed rollout is final, so its last turn is
  settled when it is read).
- **Rewrites.** Transcripts only ever grow, so a file whose size and stamp
  (modification time, and on Unix device and inode) are unchanged is not read
  (a stat per file per poll). One written to without growing — rewritten in
  place, or replaced, at exactly its old size — has a new stamp and is read
  again whole and sent again; so is one merely touched, which costs a harmless
  resend. A rewrite at the same size, on the same inode, within the
  filesystem's timestamp resolution (a second on some filesystems) is not
  noticed. A state file written before stamps were recorded gets each file's
  stamp on its first poll, vouched for by the file's head; it re-sends every
  Claude Code sub-agent transcript once, as the meta file's stamp is new too.
- **Prices.** The cache counters are priced only when the
  [pricing table](../configuration.md#model-pricing) names a model's
  `cache_read_per_mtok` and `cache_write_per_mtok`: then a Claude Code call's
  cache reads and writes are charged at their own rates on top of its
  uncached input, and a Codex call's cached input is carved out of its input
  at the cache rate (see [pricing cached tokens](xray.md#pricing-cached-tokens)).
  Without them the error runs in **both directions**: Anthropic's
  `input_tokens` is the uncached remainder, so cache reads and writes go
  unpriced and a cache-heavy Claude Code session's derived cost is
  **understated**; OpenAI's `input_tokens` *includes* the cached part, so
  cached input is charged at the full input rate and a cache-heavy Codex
  session's derived cost is **overstated** — with 80% of the prompt cached at a
  90% discount, the input share comes out about 3.6 times the real figure, and
  approaches 10 times as the cached share nears all of it.
- **What a transcript leaves out.** Neither tool writes its system prompt or
  tool definitions into the transcript, so the [context X-Ray](xray.md),
  which rebuilds each call's context from the conversation's recorded deltas,
  shows the conversation only, and the Cache Autopsy judges the recorded part
  of the prefix.
- **Counting.** A session rollup's `llm_calls` counts every span carrying
  `gen_ai.operation.name`, tool and agent spans included (Traza's existing rule
  for GenAI spans); a [tape](replay.md) counts steps exactly.
- **Fuses.** Imported spans pass through the [run fuses](guard.md) like any
  ingest. A historical import fills the guard's bounded live state and may warn
  on old runs; a live one is the point.
- **Size.** One trace per session means a very long session is a very large
  trace. Each file is capped at 200,000 spans (`capped` counts what was left
  out), a compressed rollout at 2 GiB read and 2 GiB decompressed, a
  sub-agent's meta file at 64 KiB, and a line at 64 MiB: a longer line is
  skipped and counted as malformed, and the lines after it are read as usual.
  Every one of these reads stops at its cap rather than trusting a length
  looked at first, so a file growing meanwhile cannot outrun it. What a
  converter remembers beside its spans is bounded by entries and by bytes:
  the digests (16 bytes each) of the most recent 16,384 Claude Code content
  blocks — enough to recognise a block an older build wrote twice — and of
  the most recent 4,096 settled Codex turns — enough to recognise an item that
  arrives after its turn ended — record timestamps for up to 1,000,000 records
  by digest, and a Codex turn's held items at 10,000 and at 256 MiB of the
  lines they came from. What a bound forgets or drops is counted in the
  `records` counts (`dedup/forgotten`, `closed_turns/forgotten`,
  `item_completed/overflow`, `token_count/unattributed`).
- **Blame** cannot see lines written by shell commands (`sed -i`, heredocs,
  generators), edits by a tool whose name is not in the list above (a patch
  applied through some other tool is found only when the importer marked it),
  edits whose arguments were not recorded, or the part of an argument
  offloaded to the payload store beyond its inline preview. Scoped to a
  repository, it misses sessions recorded under a different path for the same
  repository (another machine, a moved checkout) — `--anywhere` finds those,
  and with them same-named files of other repositories. An edit's outcome is
  the step's own record: a step marked `error` is taken to have written
  nothing (true of the all-or-nothing edit tools above), and a producer that
  marks a failed edit `ok`, or records a result for it with no status, is
  believed.
