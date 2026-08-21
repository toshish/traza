Demos
=====

Six scripted demos, one claim each. Every number they print is measured while
you watch — a timed request, a counter, a field the server answered with —
and each script checks its own claims and exits non-zero if the store stops
backing them. CI runs them all for the same reason.

Each is one command from the repo root, works in a throwaway directory, and
cleans up after itself. They build what they need on first run.

| Demo | The claim it proves | Runs for |
|---|---|---|
| [`swarm/`](swarm/README.md) | From nothing to a live agent cockpit — waterfalls, conversations, live tail, cost — one process, under a minute | until Ctrl-C |
| [`crash/`](crash/README.md) | `kill -9` mid-ingest loses nothing acknowledged; a flipped byte is named by file; restore is one flag | ~5 s |
| [`needle/`](needle/README.md) | A million spans over real HTTP, then answers in microseconds to milliseconds — latencies measured on your machine | ~10 s |
| [`incident/`](incident/README.md) | An agent investigates a runaway session over MCP: a diagnosis with evidence, a defused prompt injection, failures promoted into a dataset, a fix proven by an experiment diff | ~30 s |
| [`vanish/`](vanish/README.md) | Two tenants, one deletion request: tenant-precise erasure with a receipt that catches the backup pin still holding the bytes | ~5 s |
| [`mcp-demo/`](mcp-demo/README.md) | The whole MCP tool surface, including everything it refuses to do | ~30 s |

Pick by what you came to see:

- **"Show me it working."** `swarm/`. Leave it running and click around the dashboard.
- **"Show me it's a real database."** `crash/`, then `needle/`.
- **"Show me the agent story."** `incident/`, then `mcp-demo/`.
- **"Show me the obligations."** `vanish/`.

The dashboard demos (`swarm/` above all) want a built UI: `cd ui && npm ci &&
npm run build`, once — the server picks it up without a restart. Everything
else needs only the binary, `python3` and `curl`.
