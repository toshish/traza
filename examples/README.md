Demos
=====

Twelve scripted demos, one claim each. Every number they print is measured while
you watch — a timed request, a counter, a field the server answered with —
and each script checks its own claims and exits non-zero if the store stops
backing them. CI runs them all for the same reason.

Each is one command from the repo root, works in a throwaway directory, and
cleans up after itself. They build what they need on first run; `guard/`,
`replay/`, `prove/`, `foresight/`, `coding-agents/` and `mission-control/`
also run a prebuilt binary named by `TRAZA_BIN` (a path, or a command on
`PATH`).

| Demo | The claim it proves | Runs for |
|---|---|---|
| [`swarm/`](swarm/README.md) | From nothing to a live agent cockpit — waterfalls, conversations, live tail, cost — one process, under a minute | until Ctrl-C |
| [`crash/`](crash/README.md) | `kill -9` mid-ingest loses nothing acknowledged; a flipped byte is named by file; restore is one flag | ~5 s |
| [`needle/`](needle/README.md) | A million spans over real HTTP, then answers in microseconds to milliseconds — latencies measured on your machine | ~10 s |
| [`incident/`](incident/README.md) | An agent investigates a runaway session over MCP: a diagnosis with evidence, a defused prompt injection, failures promoted into a dataset, a fix proven by an experiment diff | ~30 s |
| [`vanish/`](vanish/README.md) | Two tenants, one deletion request: tenant-precise erasure with a receipt that catches the backup pin still holding the bytes | ~5 s |
| [`mcp-demo/`](mcp-demo/README.md) | The whole MCP tool surface, including everything it refuses to do | ~30 s |
| [`guard/`](guard/README.md) | A looping agent stopped between steps: the ack says stop, the pre-call check refuses the next step, the kill switch stops a healthy run, a grant approves more budget — and the same fuse blocks a Claude Code session | ~3 s |
| [`replay/`](replay/README.md) | A recorded run replays through the agent's own OpenAI-wire code with zero model calls; a changed prompt is refused, naming the step; a diff names where two runs parted | ~2 s |
| [`prove/`](prove/README.md) | A hash chain over everything committed, recomputed with `hashlib`; an anchor; an erasure ledgered by digest, not by name; one flipped byte named offline, exit 2 | ~2 s |
| [`foresight/`](foresight/README.md) | A run's fate, read from every run before it: the Fate Map's fatal fork with its Wilson interval, a live run called likely to fail before it has failed, a predictive fuse stopping it at the pre-call check while a healthy run goes through, and the map's own calibration | ~2 s |
| [`coding-agents/`](coding-agents/README.md) | Claude Code and Codex transcripts imported with no setup: cost per sub-agent, the cache autopsy, an X-Ray of the instruction a compaction dropped, a test edited to pass found by a detector, lines blamed to the session, step and prompt that wrote them, and `--watch` following a live transcript | ~3 s |
| [`mission-control/`](mission-control/README.md) | Live agent runs you can steer: foresight ranking the runs likely to fail, a run held by its history, a nudge the agent hears at its next check, an approval with lower odds, the kill switch, agents bouncing a ticket stopped at three handoff cycles, and a fork at an edited tool result | ~20 s, then until Ctrl-C |

Pick by what you came to see:

- **"Show me it working."** `swarm/`. Leave it running and click around the dashboard.
- **"Show me it's a real database."** `crash/`, then `needle/`.
- **"Show me the agent story."** `incident/`, then `mcp-demo/`.
- **"Show me the flight recorder."** `guard/`, then `replay/`.
- **"Show me what happens next."** `foresight/`, then `mission-control/`.
- **"Show me my coding agents."** `coding-agents/`.
- **"Show me the obligations."** `vanish/`, then `prove/`.

The dashboard demos (`swarm/` and `mission-control/` above all) want a built
UI: `cd ui && npm ci && npm run build`, once — the server picks it up without
a restart. Everything else needs only the binary, `python3` and `curl`.

Integrations
------------

Not demos — pieces to wire into your own setup:

- [`claude-code/`](claude-code/README.md): the Claude Code hook pack — a
  [run fuse](../docs/guide/guard.md) in front of every tool call, a person's
  [nudges](../docs/guide/steer.md#nudges) relayed before the next step, a
  summary of earlier runs at session start, and an installer that merges it
  into `.claude/settings.json`. The `guard/` demo runs it in its last beat.
