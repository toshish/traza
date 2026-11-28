/**
 * Errors thrown by the client.
 *
 * Every refusal the server answers with an HTTP status becomes a
 * {@link TrazaError}, or the subclass for its status. {@link FuseTripped} is
 * deliberately not one of them: a `stop` verdict is the answer to the question
 * the guard asked, not a failed request, and code that catches `TrazaError`
 * to ride out an unreachable server must not swallow it by accident.
 *
 * @module
 */

import type { QuotaRefusal, Verdict } from "./types.js";

/** Options for constructing a {@link TrazaError}. */
export interface TrazaErrorOptions {
  status?: number | null;
  type?: string | null;
  body?: unknown;
  headers?: Record<string, string>;
  cause?: unknown;
}

/**
 * A request the server refused, one that never reached it, or one whose
 * answer could not be read.
 *
 * `status` is the HTTP status, or `null` when no response arrived
 * (connection refused, DNS failure, timeout). A redirect is one of these too,
 * with its 3xx status: the client follows none. `type` is the
 * machine-readable kind some routes send beside the message, such as
 * `replay_divergence`. `body` is the parsed JSON body, or the raw text when
 * it was not JSON (an unreadable answer such as a proxy's HTML page carries
 * the text that arrived).
 */
export class TrazaError extends Error {
  readonly status: number | null;
  readonly type: string | null;
  readonly body: unknown;
  /** Response headers, names lowercased. */
  readonly headers: Record<string, string>;

  constructor(message: string, options: TrazaErrorOptions = {}) {
    super(message, options.cause === undefined ? undefined : { cause: options.cause });
    this.name = new.target.name;
    this.status = options.status ?? null;
    this.type = options.type ?? null;
    this.body = options.body;
    this.headers = options.headers ?? {};
  }

  /**
   * `false` when the server sent `x-should-retry: false`: a strict replay's
   * divergence, an exhausted or withdrawn replay, a run with no recorded
   * answer. The same request can never get past those.
   */
  get retryable(): boolean {
    return (this.headers["x-should-retry"] ?? "").toLowerCase() !== "false";
  }
}

/**
 * `401` (no, unknown, expired or revoked token) or `403`. For a `403` from
 * the auth gate, `required` names the capability the credential lacked:
 * `rw`, `admin` or `unbound`.
 */
export class AuthError extends TrazaError {
  get required(): string | null {
    const body = this.body as { required?: unknown } | null;
    return body && typeof body === "object" && typeof body.required === "string" ? body.required : null;
  }
}

/** `404`: no such run, replay, policy or route — another tenant's included. */
export class NotFound extends TrazaError {}

/**
 * `409`: a run that cannot be replayed, a strict replay's divergence, or a
 * `/v1/fuses` route on a server started with `--no-fuses`. Check
 * {@link TrazaError.retryable} before retrying.
 */
export class Conflict extends TrazaError {}

/** `410`: a replay an erasure withdrew, or one with nothing left to serve. */
export class Gone extends TrazaError {}

/**
 * `429`: a tenant quota or ingest pause, or no room for another replay.
 * `retryAfter` is the server's `Retry-After` in seconds. `quota` is the
 * refusal's quota object for a quota or pause, and `null` for a replay
 * refused for want of room.
 */
export class QuotaExceeded extends TrazaError {
  get retryAfter(): number | null {
    const value = this.headers["retry-after"];
    if (value === undefined) return null;
    const seconds = Number(value);
    // RFC 9110 also allows an HTTP-date; Traza sends seconds, so anything
    // else is reported as absent rather than guessed at.
    return Number.isFinite(seconds) ? seconds : null;
  }

  get quota(): QuotaRefusal | null {
    const body = this.body as { quota?: unknown } | null;
    return body && typeof body === "object" && body.quota && typeof body.quota === "object"
      ? (body.quota as QuotaRefusal)
      : null;
  }
}

/**
 * The run's fuse answered `stop`: the next step must not run. Thrown by
 * {@link Client.guard} and functions wrapped by {@link Client.guarded} before
 * the step executes. `verdict` is the full verdict document.
 */
export class FuseTripped extends Error {
  readonly verdict: Verdict;

  constructor(verdict: Verdict) {
    super(`run ${JSON.stringify(verdict.run)} is stopped: ${verdict.reasons.join("; ") || "stopped"}`);
    this.name = "FuseTripped";
    this.verdict = verdict;
  }

  get reasons(): string[] {
    return [...this.verdict.reasons];
  }
}

const BY_STATUS: Record<number, typeof TrazaError> = {
  401: AuthError,
  403: AuthError,
  404: NotFound,
  409: Conflict,
  410: Gone,
  429: QuotaExceeded,
};

/**
 * Builds the error for a non-2xx response. Traza's own routes answer
 * `{"error": "<message>"}` (with `type` beside it on the replay routes); the
 * provider-shaped replay routes answer `{"error": {"type", "message", ...}}`,
 * wrapped in `{"type": "error"}` on the Anthropic route. All three are read.
 */
export function errorFromResponse(
  status: number,
  statusText: string,
  headers: Record<string, string>,
  text: string,
): TrazaError {
  let body: unknown = text;
  let message = text.trim() || statusText || `HTTP ${status}`;
  let type: string | null = null;
  try {
    body = text ? JSON.parse(text) : null;
  } catch {
    // Not JSON (a proxy's HTML error page, say): keep the text.
  }
  if (body && typeof body === "object" && !Array.isArray(body)) {
    const error = (body as Record<string, unknown>).error;
    if (typeof error === "string") {
      message = error;
      const kind = (body as Record<string, unknown>).type;
      type = typeof kind === "string" ? kind : null;
    } else if (error && typeof error === "object") {
      const detail = error as Record<string, unknown>;
      if (typeof detail.message === "string") message = detail.message;
      type = typeof detail.type === "string" ? detail.type : null;
    }
  }
  const ErrorClass = BY_STATUS[status] ?? TrazaError;
  return new ErrorClass(message, { status, type, body, headers });
}
