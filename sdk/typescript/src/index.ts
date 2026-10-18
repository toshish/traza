/**
 * A typed client for TrazaDB's agent-native APIs.
 *
 * Not instrumentation — OpenTelemetry records the spans ({@link otelEnv}
 * writes its exporter settings). This package asks the server about what was
 * recorded: run fuses and the pre-call guard, tapes and replays, diffs,
 * recall, the ledger and evidence packs — and it forks a recorded run at any
 * step ({@link Client.fork}) and replays recorded runs as tests
 * ({@link replayTest}). No runtime dependencies: the global `fetch` is the
 * transport.
 *
 * ```ts
 * import { Client } from "@trazadb/sdk";
 *
 * const traza = new Client("http://127.0.0.1:8080", { token: process.env.TRAZA_TOKEN });
 * await traza.guard("sess-7f3a", { estCostUsd: 0.02 }); // throws FuseTripped on stop
 * ```
 *
 * @module
 */

export { Client, EventStream, FOLLOW_WAIT_MS, Replay } from "./client.js";
export type {
  CallOptions,
  CheckOptions,
  ClientOptions,
  EventOptions,
  FetchLike,
  GuardOptions,
  RequestOptions,
} from "./client.js";
export { AuthError, Conflict, FuseTripped, Gone, NotFound, QuotaExceeded, TrazaError } from "./errors.js";
export { FORK_ID, FORK_PARENT, FORK_STEP, Fork } from "./fork.js";
export type { ForkOptions, ForkSpanProcessor, LiveProvider, TaggableSpan } from "./fork.js";
export { describeReplay, replayTest, TapeMismatch } from "./testing.js";
export type { ReplayTestOptions } from "./testing.js";
export type { TrazaErrorOptions } from "./errors.js";
export { verifyEvidenceDigest } from "./evidence.js";
export { parseJson, RawNumber, stringifyJson } from "./json.js";
export { otelEnv } from "./otel.js";
export type { OtelEnvOptions } from "./otel.js";
export type * from "./types.js";

/** This package's version, which tracks the engine release it was built against. */
export const VERSION = "0.27.0";
