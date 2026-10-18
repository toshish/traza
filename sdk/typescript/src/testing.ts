/**
 * Tape tests: prove an agent still takes a recorded path, with zero model calls.
 *
 * ```ts
 * import { test } from "vitest";            // or jest, or node:test
 * import { Client, replayTest } from "@trazadb/sdk";
 *
 * test("the weather agent still asks for the tool", async () => {
 *   await replayTest(new Client(url), "weather-7", async (replay) => {
 *     await runAgent(new OpenAI({ baseURL: replay.openaiBaseUrl, apiKey: "traza" }));
 *   });
 * });
 * ```
 *
 * Every model call the agent makes is answered from the recording, and must
 * ask exactly what the recording asked. {@link replayTest} throws a
 * {@link TapeMismatch} — which every test runner reports as a failure — when
 * the agent's request at some step differs (naming the message, field and
 * character where it first differs), when it stops before the recording's
 * last model call, or when it asks for more than the recording holds.
 *
 * @module
 */

import type { CallOptions, Client, Replay } from "./client.js";
import type { Difference, ReplayStatus } from "./types.js";

/** The agent did not take the recorded path. `status` is the replay's status, the evidence. */
export class TapeMismatch extends Error {
  override name = "TapeMismatch";
  readonly status: ReplayStatus;

  constructor(message: string, status: ReplayStatus, options?: { cause?: unknown }) {
    super(message, options);
    this.status = status;
  }
}

function quote(text: string | null | undefined): string {
  return typeof text === "string" ? JSON.stringify(text) : "nothing";
}

function differenceLines(difference: Difference): string[] {
  const role = difference.role ?? "?";
  const lines =
    difference.field === "missing"
      ? [`  message ${difference.message} (${role}) is missing from the request; the recording has ${quote(difference.recorded)}`]
      : difference.field === "extra"
        ? [`  the request has an extra message ${difference.message} (${role}): ${quote(difference.got)}`]
        : [
            `  message ${difference.message} (${role}): ${difference.field} differs at character ${difference.at_char}`,
            `    recorded: ${quote(difference.recorded)}`,
            `    got:      ${quote(difference.got)}`,
          ];
  lines.push(
    `  (the recording's request has ${difference.recorded_messages} messages, the agent's ${difference.got_messages})`,
  );
  return lines;
}

/**
 * Why a replayed run failed its tape — or `null` when the agent made exactly
 * the recorded model calls, each asking what the recording asked.
 */
export function describeReplay(status: ReplayStatus, run?: string): string | null {
  const name = run ?? status.run ?? "?";
  if (status.diverged) {
    const lines = [`the agent diverged from recorded run ${name} at step ${status.diverged.step}:`];
    if (status.diverged.difference) lines.push(...differenceLines(status.diverged.difference));
    else {
      lines.push(
        `  its request digests to ${String(status.diverged.got_digest).slice(0, 12)}, the recording's to ` +
          `${String(status.diverged.expected_digest).slice(0, 12)}`,
      );
    }
    return lines.join("\n");
  }
  const served = Number(status.served ?? 0);
  const remaining = Number(status.remaining ?? 0);
  if (remaining > 0) {
    return (
      `the agent stopped after ${served} of the ${served + remaining} model calls recorded in run ${name}: ` +
      `the recording expected ${remaining} more`
    );
  }
  if (Number(status.exhausted_calls ?? 0) > 0) {
    return `the agent made ${status.exhausted_calls} model call(s) past the end of recorded run ${name}, which holds ${served}`;
  }
  return null;
}

/** Options for {@link replayTest}. */
export interface ReplayTestOptions extends CallOptions {
  /** `strict` (default) refuses a request that differs from the recording; `loose` serves in order. */
  mode?: "strict" | "loose";
}

/**
 * Runs `agent` against a strict replay of `run` and resolves the replay's
 * status — or throws {@link TapeMismatch} saying where the agent left the
 * recorded path. An error the agent threw because it was refused (a
 * divergence answers `409`) becomes the mismatch's `cause`; any other error
 * propagates as it was.
 */
export async function replayTest(
  client: Client,
  run: string,
  agent: (replay: Replay) => unknown,
  options: ReplayTestOptions = {},
): Promise<ReplayStatus> {
  const replay = await client.replay(run, { mode: options.mode ?? "strict", signal: options.signal });
  try {
    await agent(replay);
  } catch (error) {
    const status = await replay.status({ signal: options.signal });
    const failure = describeReplay(status, run);
    if (failure !== null) throw new TapeMismatch(failure, status, { cause: error });
    throw error;
  }
  const status = await replay.status({ signal: options.signal });
  const failure = describeReplay(status, run);
  if (failure !== null) throw new TapeMismatch(failure, status);
  return status;
}
