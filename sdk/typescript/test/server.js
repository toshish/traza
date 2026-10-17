// The real traza-server on an ephemeral loopback port, for the client tests.
//
// Hermetic like the engine's own harness (tests/common/mod.rs): a private
// data directory, `--host 127.0.0.1 --port 0`, and an environment with every
// inherited TRAZA_* variable removed, so a developer's exported TRAZA_TOKENS
// cannot turn an open-mode test into an auth test. The binary is
// $TRAZA_SERVER_BIN, else the release build at the repository root.

import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const DEFAULT_BIN = fileURLToPath(new URL("../../../target/release/traza-server", import.meta.url));
const LISTENING = /traza-server listening on 127\.0\.0\.1:(\d+)/;

export function serverBinary() {
  const path = resolve(process.env.TRAZA_SERVER_BIN || DEFAULT_BIN);
  if (!existsSync(path)) {
    // A failure, not a skip: a suite that skips itself when the binary is
    // missing would let CI pass without testing anything.
    throw new Error(
      `traza-server not found at ${path}; build it with \`cargo build --release\` ` +
        "or point TRAZA_SERVER_BIN at a built binary",
    );
  }
  return path;
}

/** Starts a server; resolves `{ url, port, stderr(), stop() }`. */
export async function startServer({ args = [], env = {} } = {}) {
  const dir = mkdtempSync(join(tmpdir(), "traza-sdk-ts-"));
  const childEnv = Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.startsWith("TRAZA_")));
  Object.assign(childEnv, env);
  const child = spawn(
    serverBinary(),
    ["--data-dir", join(dir, "store"), "--host", "127.0.0.1", "--port", "0", ...args],
    { stdio: ["ignore", "ignore", "pipe"], env: childEnv },
  );
  let stderr = "";
  let exited = false;
  // The listener keeps draining stderr after the port is read: a server
  // blocked on a full pipe looks hung.
  child.stderr.setEncoding("utf8");
  child.stderr.on("data", (chunk) => {
    stderr += chunk;
  });
  const exit = new Promise((resolveExit) => {
    child.on("exit", () => {
      exited = true;
      resolveExit();
    });
  });
  const port = await new Promise((resolvePort, reject) => {
    const timer = setTimeout(() => reject(new Error(`traza-server did not report a port within 15 s:\n${stderr}`)), 15_000);
    const poll = setInterval(() => {
      const match = LISTENING.exec(stderr);
      if (match) {
        clearTimeout(timer);
        clearInterval(poll);
        resolvePort(Number(match[1]));
      } else if (exited) {
        clearTimeout(timer);
        clearInterval(poll);
        reject(new Error(`traza-server exited before listening:\n${stderr}`));
      }
    }, 10);
  });
  return {
    port,
    url: `http://127.0.0.1:${port}`,
    stderr: () => stderr,
    async stop() {
      if (!exited) {
        child.kill("SIGTERM");
        const killer = setTimeout(() => child.kill("SIGKILL"), 5_000);
        await exit;
        clearTimeout(killer);
      }
      rmSync(dir, { recursive: true, force: true });
    },
  };
}
