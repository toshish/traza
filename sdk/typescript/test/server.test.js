// The test harness's own failure paths. A harness that hangs when the server
// never comes up turns a one-line failure into a CI job sitting at its
// 45-minute timeout, so each way startServer can fail is driven here: in a
// child process, which must exit promptly, leave no server running and no
// temporary directory behind.

import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { chmodSync, existsSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, describe, it } from "node:test";

const HARNESS = new URL("./server.js", import.meta.url).href;
const scratch = mkdtempSync(join(tmpdir(), "traza-sdk-harness-"));

after(() => rmSync(scratch, { recursive: true, force: true }));

/** Runs startServer against `binary` in a child node; resolves how it ended. */
async function startIn(binary, startupTimeoutMs) {
  const temp = mkdtempSync(join(scratch, "tmp-"));
  const script =
    `import { startServer } from ${JSON.stringify(HARNESS)};` +
    `try { const s = await startServer({ startupTimeoutMs: ${startupTimeoutMs} }); console.log("started"); await s.stop(); }` +
    `catch (error) { console.log("rejected: " + error.message.split("\\n")[0]); }`;
  const child = spawn(process.execPath, ["--input-type=module", "-e", script], {
    env: { ...process.env, TRAZA_SERVER_BIN: binary, TMPDIR: temp },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let stdout = "";
  let stderr = "";
  child.stdout.on("data", (chunk) => (stdout += chunk));
  child.stderr.on("data", (chunk) => (stderr += chunk));
  const began = Date.now();
  // Well past the harness's own 15 s default, so a harness that ignores the
  // short timeout and then hangs is told apart from one that merely waited.
  const deadline = setTimeout(() => child.kill("SIGKILL"), 25_000);
  const [code, signal] = await new Promise((resolve) => child.on("exit", (...ended) => resolve(ended)));
  clearTimeout(deadline);
  return { code, signal, stdout, stderr, seconds: (Date.now() - began) / 1000, leftovers: readdirSync(temp) };
}

function script(name, body) {
  const path = join(scratch, name);
  writeFileSync(path, `#!/bin/sh\n${body}\n`);
  chmodSync(path, 0o755);
  return path;
}

function alive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

describe("the server harness when the server never listens", () => {
  it("gives up at its startup timeout, killing the server and removing its directory", async () => {
    const pidFile = join(scratch, "silent.pid");
    const silent = script("silent", `echo $$ > ${JSON.stringify(pidFile)}\nexec sleep 60`);
    const ended = await startIn(silent, 300);
    // Absent only if the harness killed the script before its first line ran.
    const pid = existsSync(pidFile) ? Number(readFileSync(pidFile, "utf8")) : null;
    try {
      assert.equal(ended.signal, null, `the harness hung until killed:\n${ended.stdout}${ended.stderr}`);
      assert.match(ended.stdout, /^rejected: traza-server did not report a port/);
      assert.ok(ended.seconds < 10, `took ${ended.seconds} s`);
      if (pid !== null) assert.equal(alive(pid), false, "the server outlived the harness");
      assert.deepEqual(ended.leftovers, []);
    } finally {
      if (pid !== null && alive(pid)) process.kill(pid, "SIGKILL");
    }
  });

  it("reports a server that exits first, and removes its directory", async () => {
    const ended = await startIn(script("exits", "echo 'no such flag' >&2\nexit 3"), 15_000);
    assert.equal(ended.signal, null);
    assert.match(ended.stdout, /^rejected: traza-server exited before listening/);
    assert.deepEqual(ended.leftovers, []);
  });

  it("reports a binary that cannot be started, and removes its directory", async () => {
    const path = join(scratch, "not-executable");
    writeFileSync(path, "#!/bin/sh\n");
    chmodSync(path, 0o644);
    const ended = await startIn(path, 15_000);
    assert.equal(ended.signal, null);
    assert.match(ended.stdout, /^rejected: could not start traza-server/, ended.stderr);
    assert.deepEqual(ended.leftovers, []);
  });
});
