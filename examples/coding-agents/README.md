# Coding-agents demo

Claude Code and Codex sessions, recorded from the transcripts both tools
already write, and read back: what each agent and sub-agent cost, why the
prompt cache broke, what a compaction took out of the model's context, which
session edited a test to make it pass, which session, step and prompt wrote
a given line — and a live session followed as its transcript grows.

```sh
examples/coding-agents/run.sh
```

It builds `traza-server` if it is missing (or runs the binary `TRAZA_BIN`
names), starts it on port 8133 with one `rw` token and a `--pricing` file,
and deletes the store, the transcripts and the project on exit. `python3`
writes the transcripts and reads replies; `curl` carries the requests. **No
real transcript is read**: the importer is pointed at a throwaway directory
holding only what `transcripts.py` wrote.

## The transcripts

`transcripts.py` authors four sessions on one small Python shop, in the
shapes Claude Code 2.x and the Codex CLI write to `~/.claude/projects` and
`~/.codex/sessions` (the shapes of the importer's own fixtures in
`tests/fixtures/import/`, documented on `src/import/mod.rs`), timestamped
relative to the moment the demo runs. It also writes the shop's files as the
sessions left them, so blame can read real lines.

| Session | Tool | What happens |
|---|---|---|
| coupons | Claude Code | Adds coupon codes. An `Explore` sub-agent (Haiku) maps the code, a `code-reviewer` sub-agent asks for a cap. A 16-minute pause lets the prompt cache expire. A `/compact` summarizes the session — and drops the first prompt's rule, *never touch anything under migrations/*. Two calls later the agent writes a migration. |
| tests | Claude Code | Asked to fix a failing test, edits the test's expectation instead of the code. |
| rename | Claude Code | Renames a variable and stops without running the tests. |
| endpoint | Codex | Adds an API route with `apply_patch` and runs the tests. |

## The beats

| Beat | What to watch for |
|---|---|
| 1 · import | `traza-server import claude-code` and `import codex`: 4 sessions, 31 model calls, 23 tool calls, 2 sub-agents, every record understood (no unknown or malformed records — asserted). A second import with the same state file sends nothing. |
| 2 · cost | `GET /v1/runs/{session}/topology` per session, summed by agent: the main Claude Code loop, the two sub-agents, Codex. Every agent's cost is recomputed from the transcripts' own token counts and the pricing file — Anthropic usage once per message, cache reads and writes at their rates; Codex's cached input carved out at the cache rate — and must equal the server's (asserted). |
| 3 · cache autopsy | `GET /v1/cache/{coupons}`: 19 model calls in three conversations. Two breaks: `ttl_expired` after the pause, priced from the pricing file, and `new_context · rewritten` at the `/compact` — with the server's advice for each. |
| 4 · X-Ray | `GET /v1/xray/{trace}/{span}` for the call before the compaction — 22 chunks rebuilt from the transcript's deltas, each with the step that introduced it (the Explore report traced to its `Agent` call, test output to its `Bash` call) and its age — then for the call after it: 2 chunks, and `dropped_since_previous` listing the instructions the compaction took, the migrations rule among them (asserted). |
| 5 · behaviour | `GET /v1/detectors/findings`: `test_tampering` in the tests session — the failing test run, the edit to the test (its diff shown), the passing run — and `unverified_finish` in two sessions. Then one query of our own, `edit[file~"(^|/)migrations/"]`: the coupons session, three steps after the compaction (asserted). |
| 6 · blame | `traza-server blame FILE:LINE`, run inside the shop: the tampered assertion → the tests session's `Edit`; the migration → the coupons session's `Write` and the prompt that asked for it; the API route → the Codex session's `apply_patch` (asserted). |
| 7 · `--watch` | An `import --watch` follows the transcripts while the rename session resumes: a prompt and a `Bash` call appended to its file appear on its tape within a poll (the time is measured and printed), the session shows in `GET /v1/runs/live` running `Bash`, and once its tests ran, `unverified_finish` no longer matches it (asserted). |

## Knobs

| Variable | Default | Meaning |
|---|---|---|
| `TRAZA_DEMO_PORT` | `8133` | Server port |
| `TRAZA_BIN` | `./target/release/traza-server` | The server binary: a path, taken from your current directory, or a command name on `PATH`. The default is built with cargo when missing |

## Honest caveats

- **The transcripts are synthetic**, written to tell one story in a few
  sessions. Their token counts are authored too, so the costs printed are
  this demo's arithmetic on made-up usage — checked against an independent
  recomputation, but not a measurement of either tool.
- **A transcript is not the whole request.** Neither tool writes its system
  prompt or tool definitions down, so the X-Ray shows the conversation only
  and the autopsy prices the recorded part of a lost prefix; the provider's
  counts, printed beside them, cover more.
- **Outcomes are inferred.** No session declares one, so each reads as how
  its last span ended — which is why the test-tampering session reads as a
  success. The detectors are how you find it.
- **The pricing file is the demo's.** Its rates are chosen for the example;
  set your own with [`--pricing`](../../docs/configuration.md#model-pricing).
- **Blame sees recorded edits only** — the editing tools' arguments, Codex
  patches — not lines written by shell commands.

## Runtime

About 3 s on an Apple M1 Max laptop, the `--watch` beat included. There is no
scale knob: CI runs it as is.
