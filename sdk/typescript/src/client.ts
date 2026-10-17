/**
 * The HTTP client for Traza's agent-native routes.
 *
 * Built on the global `fetch`, so it runs unchanged in Node 20+, Deno, Bun,
 * edge runtimes and browsers. The client keeps no state between calls beyond
 * its configuration.
 *
 * @module
 */

import { errorFromResponse, FuseTripped, TrazaError } from "./errors.js";
import { verifyEvidenceDigest } from "./evidence.js";
import { parseJson, stringifyJson } from "./json.js";
import type {
  Diff,
  EvidencePack,
  FuseEvent,
  Identity,
  LedgerHead,
  Limits,
  Policies,
  Policy,
  Recall,
  ReplayDocument,
  ReplayEnv,
  ReplayStatus,
  Tape,
  ToolStep,
  UsageReport,
  Verdict,
} from "./types.js";

/** The `fetch` signature the client needs; pass your own to instrument or mock. */
export type FetchLike = (input: string, init: RequestInit) => Promise<Response>;

/** Options for {@link Client}. */
export interface ClientOptions {
  /** Sent as `Authorization: Bearer`; leave it out for an open server. */
  token?: string;
  /**
   * Scopes every call that takes a tenant, for an unbound credential. A bound
   * credential is held to its own tenant whatever is sent, and naming
   * another is refused. `""` names the default tenant.
   */
  tenant?: string;
  /** Per request, in milliseconds. Default 10000. A long-poll adds its wait. */
  timeoutMs?: number;
  /** A `fetch` to use instead of the global one. */
  fetch?: FetchLike;
}

/** Every method accepts a signal to cancel it. */
export interface CallOptions {
  signal?: AbortSignal;
}

/** Options for {@link Client.check}, {@link Client.guard} and {@link Client.guarded}. */
export interface CheckOptions extends CallOptions {
  /** The next step's estimated cost, in US dollars. */
  estCostUsd?: number;
  /** The next step's estimated tokens. */
  estTokens?: number;
  /** The tool the next step calls; without it the step is an LLM call. */
  tool?: string;
}

/** Options for {@link Client.guard} and {@link Client.guarded}. */
export interface GuardOptions extends CheckOptions {
  /**
   * When the check cannot be answered (unreachable server, refused check),
   * let the step run instead of throwing. Off by default: running unguarded
   * is a decision the caller should make, not a silent fallback.
   */
  failOpen?: boolean;
  /** Called with the error a fail-open guard swallowed. */
  onError?: (error: TrazaError) => void;
}

/** Options for {@link Client.events}. */
export interface EventOptions extends CallOptions {
  /** The cursor to read after; absent reads everything the server retains. */
  after?: string;
  /** How long each read waits for an event when none is there, 0–30000 ms. */
  waitMs?: number;
  /** Keep polling forever instead of ending at the first empty read. */
  follow?: boolean;
}

/** Options for {@link Client.request}. */
export interface RequestOptions extends CallOptions {
  /** Query parameters; `undefined` and `null` values are left out. */
  query?: Record<string, string | number | bigint | boolean | null | undefined>;
  /** A JSON body; a `bigint` is written as an exact JSON integer. */
  body?: unknown;
  /** Overrides the client's per-request timeout. */
  timeoutMs?: number;
}

const DEFAULT_URL = "http://127.0.0.1:8080";

/** A client for one Traza server. */
export class Client {
  /** The server's base URL, without a trailing slash. */
  readonly url: string;
  readonly tenant: string | undefined;
  readonly timeoutMs: number;
  // A private field rather than a property: console.log(client) and
  // JSON.stringify(client) must never print the token.
  readonly #token: string | undefined;
  readonly #fetch: FetchLike | undefined;

  constructor(url: string = DEFAULT_URL, options: ClientOptions = {}) {
    let parsed: URL;
    try {
      parsed = new URL(url);
    } catch {
      throw new TypeError(`url must be an http:// or https:// URL, got ${JSON.stringify(url)}`);
    }
    if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
      throw new TypeError(`url must be an http:// or https:// URL, got ${JSON.stringify(url)}`);
    }
    this.url = url.replace(/\/+$/, "");
    this.#token = options.token || undefined;
    this.tenant = options.tenant;
    this.timeoutMs = options.timeoutMs ?? 10_000;
    this.#fetch = options.fetch;
  }

  /**
   * A client from `TRAZA_URL`, `TRAZA_TOKEN` and `TRAZA_TENANT` — the
   * variables the Claude Code hook in `examples/guard` reads. Defaults to
   * `process.env` where there is one.
   */
  static fromEnv(env?: Record<string, string | undefined>): Client {
    const source =
      env ?? (globalThis as { process?: { env?: Record<string, string | undefined> } }).process?.env ?? {};
    return new Client(source.TRAZA_URL || DEFAULT_URL, {
      token: source.TRAZA_TOKEN || undefined,
      tenant: source.TRAZA_TENANT,
    });
  }

  /** The bearer token, if any. */
  get token(): string | undefined {
    return this.#token;
  }

  // ---------------------------------------------------------- identity

  /** What the calling credential is: scope, binding, operator or not. */
  whoami(options: CallOptions = {}): Promise<Identity> {
    return this.request<Identity>("GET", "/v1/whoami", options);
  }

  /**
   * Ingest counters, limits and quota state, per tenant. A bound credential
   * gets its own row; an unbound one every tenant, or the client's tenant.
   */
  usage(options: CallOptions = {}): Promise<UsageReport> {
    return this.request<UsageReport>("GET", "/v1/usage", { ...options, query: this.#tenantQuery() });
  }

  // ---------------------------------------------------------- fuses

  /**
   * The pre-call gate: may `run` take its next step? The step — an LLM call,
   * or a call of `tool` — is added to the run's recorded usage
   * hypothetically, with the estimates; a step that would go past a `stop`
   * limit answers `stop`. Nothing about the run changes, and a `stop` is a
   * normal answer here: {@link guard} is the form that throws.
   */
  check(run: string, options: CheckOptions = {}): Promise<Verdict> {
    const next: Record<string, unknown> = {};
    if (options.estCostUsd !== undefined) next.est_cost_usd = options.estCostUsd;
    if (options.estTokens !== undefined) next.est_tokens = options.estTokens;
    if (options.tool !== undefined) next.tool = options.tool;
    const body: Record<string, unknown> = { run, next };
    if (this.tenant !== undefined) body.tenant = this.tenant;
    return this.request<Verdict>("POST", "/v1/fuses/check", { signal: options.signal, body });
  }

  /**
   * Checks before a step and throws {@link FuseTripped} on `stop`, so the
   * step never starts. Resolves the verdict otherwise (`continue` or
   * `warn`), or `null` when a fail-open check could not be answered.
   *
   * ```ts
   * await traza.guard(runId, { tool: "search" });
   * const hits = await search(query);
   * ```
   */
  async guard(run: string, options: GuardOptions = {}): Promise<Verdict | null> {
    let verdict: Verdict;
    try {
      verdict = await this.check(run, options);
    } catch (error) {
      if (options.failOpen && error instanceof TrazaError) {
        options.onError?.(error);
        return null;
      }
      throw error;
    }
    if (verdict.verdict === "stop") throw new FuseTripped(verdict);
    return verdict;
  }

  /**
   * Wraps `fn` so every call is guarded first. `run` may be a function of
   * `fn`'s arguments — for a graph node whose run id lives in its state.
   *
   * ```ts
   * const node = traza.guarded((state: State) => state.runId, agentNode);
   * ```
   */
  guarded<A extends unknown[], R>(
    run: string | ((...args: A) => string),
    fn: (...args: A) => R | Promise<R>,
    options: GuardOptions = {},
  ): (...args: A) => Promise<Awaited<R>> {
    return async (...args: A): Promise<Awaited<R>> => {
      await this.guard(typeof run === "function" ? run(...args) : run, options);
      return await fn(...args);
    };
  }

  /**
   * The run's current verdict, usage and limits. The first question about a
   * run rebuilds it from stored spans (`source: "rebuilt"`); a run no span
   * carries is `continue`.
   */
  runVerdict(run: string, options: CallOptions = {}): Promise<Verdict> {
    return this.request<Verdict>("GET", `/v1/fuses/runs/${segment(run)}`, {
      ...options,
      query: this.#tenantQuery(),
    });
  }

  /**
   * Runs the guard holds, most recently active first, from the server's
   * memory alone. `state` is `active` (all of them), `warn` or `stop`.
   */
  async runs(
    options: CallOptions & { state?: "active" | "warn" | "stop"; limit?: number } = {},
  ): Promise<Verdict[]> {
    const page = await this.request<{ runs: Verdict[] }>("GET", "/v1/fuses/runs", {
      signal: options.signal,
      query: { ...this.#tenantQuery(), state: options.state ?? "active", limit: options.limit },
    });
    return page.runs;
  }

  /**
   * The kill switch: stop `run` now, durably, until it is reset. `reason` is
   * stored as given until the run's session or tenant is erased; keep
   * personal data out of it. Needs an `rw` credential.
   */
  trip(run: string, options: CallOptions & { reason?: string } = {}): Promise<Verdict> {
    return this.request<Verdict>("POST", `/v1/fuses/runs/${segment(run)}/trip`, {
      signal: options.signal,
      query: this.#tenantQuery(),
      body: options.reason === undefined ? {} : { reason: options.reason },
    });
  }

  /**
   * Clears a stop (a policy's or the kill switch), the error streak and the
   * repeat counts. Cumulative usage is not refilled, so a run still over a
   * cumulative limit answers `stop` again at once. `grant` raises the run's
   * limits to the absolute values named.
   */
  reset(run: string, options: CallOptions & { grant?: Limits } = {}): Promise<Verdict> {
    return this.request<Verdict>("POST", `/v1/fuses/runs/${segment(run)}/reset`, {
      signal: options.signal,
      query: this.#tenantQuery(),
      body: options.grant === undefined ? {} : { grant: options.grant },
    });
  }

  /** The stored fuse policies and the server's global default. */
  policies(options: CallOptions = {}): Promise<Policies> {
    return this.request<Policies>("GET", "/v1/fuses/policies", { ...options, query: this.#tenantQuery() });
  }

  /**
   * Creates a policy, or replaces the tenant's policy of the same name.
   * `service` is required unless `name` is `default`, which must not have
   * one. Limits are not inherited from `default` or `global`.
   */
  putPolicy(
    policy: { name: string; limits: Limits; service?: string; action?: "stop" | "warn" },
    options: CallOptions = {},
  ): Promise<Policy> {
    const body: Record<string, unknown> = {
      name: policy.name,
      limits: policy.limits,
      action: policy.action ?? "warn",
    };
    if (policy.service !== undefined) body.match = { service: policy.service };
    if (this.tenant !== undefined) body.tenant = this.tenant;
    return this.request<Policy>("POST", "/v1/fuses/policies", { signal: options.signal, body });
  }

  /** Deletes a policy by id; resolves the policy as it was. */
  async deletePolicy(id: string, options: CallOptions = {}): Promise<Policy> {
    const answer = await this.request<{ deleted: boolean; policy: Policy }>(
      "POST",
      `/v1/fuses/policies/${segment(id)}/delete`,
      options,
    );
    return answer.policy;
  }

  /**
   * Verdict transitions, by cursor, as an async iterable:
   *
   * ```ts
   * for await (const event of traza.events({ follow: true, waitMs: 25_000, signal })) { … }
   * ```
   *
   * Iteration ends at the first read that returns nothing (with `waitMs: 0`,
   * once what the server retains is drained) unless `follow` is set. The
   * stream's `cursor` is where to resume with `after`; `gap` turns true when
   * the server could no longer honour a cursor.
   */
  events(options: EventOptions = {}): EventStream {
    return new EventStream(this, options);
  }

  // ---------------------------------------------------------- tapes and replays

  /**
   * The run's steps — model, tool and agent calls — in start order.
   * `resolve` inlines offloaded `$payload` values, up to the server's
   * `--tape-max-bytes`.
   */
  tape(run: string, options: CallOptions & { resolve?: boolean; maxSteps?: number } = {}): Promise<Tape> {
    return this.request<Tape>("GET", `/v1/tapes/${segment(run)}`, {
      signal: options.signal,
      query: { ...this.#tenantQuery(), resolve: options.resolve ? "true" : undefined, max_steps: options.maxSteps },
    });
  }

  /**
   * Snapshots `run` into a replay that serves its model answers back.
   * `strict` (the default) refuses a request whose messages differ from the
   * recorded step; `loose` serves in order regardless. It lives in server
   * memory for an hour.
   */
  async replay(run: string, options: CallOptions & { mode?: "strict" | "loose" } = {}): Promise<Replay> {
    const body: Record<string, unknown> = { run, mode: options.mode ?? "strict" };
    if (this.tenant !== undefined) body.tenant = this.tenant;
    const document = await this.request<ReplayDocument>("POST", "/v1/replays", { signal: options.signal, body });
    return new Replay(this, document);
  }

  /** Aligns two runs' steps and names the first place they diverge. */
  diff(a: string, b: string, options: CallOptions = {}): Promise<Diff> {
    return this.request<Diff>("GET", "/v1/diff", { signal: options.signal, query: { ...this.#tenantQuery(), a, b } });
  }

  /**
   * Earlier runs whose recorded text holds every word of `query`, successes
   * first under `outcome: "any"`.
   */
  recall(
    query: string,
    options: CallOptions & { outcome?: "any" | "success" | "failure"; limit?: number } = {},
  ): Promise<Recall> {
    return this.request<Recall>("GET", "/v1/recall", {
      signal: options.signal,
      query: { ...this.#tenantQuery(), q: query, outcome: options.outcome ?? "any", limit: options.limit ?? 5 },
    });
  }

  // ---------------------------------------------------------- ledger

  /** The ledger's tip: what to anchor with an external notary. */
  ledgerHead(options: CallOptions = {}): Promise<LedgerHead> {
    return this.request<LedgerHead>("GET", "/v1/ledger/head", options);
  }

  /**
   * The evidence pack for a window of Unix nanoseconds. A bound credential
   * gets its tenant's pack; an unbound one the client's tenant, or the whole
   * store. `verifyDigest()` on the result recomputes its digest.
   */
  async evidence(
    options: CallOptions & { since?: bigint | number; until?: bigint | number } = {},
  ): Promise<EvidencePack> {
    const text = await this.#send("GET", "/v1/evidence", {
      signal: options.signal,
      query: { ...this.#tenantQuery(), since: options.since, until: options.until },
    });
    const pack = parseJson(text) as EvidencePack;
    Object.defineProperty(pack, "verifyDigest", { value: () => verifyEvidenceDigest(text), enumerable: false });
    return pack;
  }

  // ---------------------------------------------------------- transport

  /**
   * Sends one request and resolves its parsed JSON body: the escape hatch for
   * routes this client does not wrap, with the same credential, timeout and
   * error mapping as every method above.
   */
  async request<T = unknown>(method: string, path: string, options: RequestOptions = {}): Promise<T> {
    const text = await this.#send(method, path, options);
    return (text ? parseJson(text) : null) as T;
  }

  async #send(method: string, path: string, options: RequestOptions): Promise<string> {
    let url = this.url + path;
    const query = new URLSearchParams();
    for (const [key, value] of Object.entries(options.query ?? {})) {
      if (value !== undefined && value !== null) query.append(key, String(value));
    }
    const search = query.toString();
    if (search) url += `?${search}`;

    const headers: Record<string, string> = { accept: "application/json" };
    let body: string | undefined;
    if (options.body !== undefined) {
      headers["content-type"] = "application/json";
      body = stringifyJson(options.body);
    }
    if (this.#token) headers.authorization = `Bearer ${this.#token}`;

    const timeoutMs = options.timeoutMs ?? this.timeoutMs;
    const deadline = linkSignals(options.signal, timeoutMs);
    const fetcher = this.#fetch ?? globalThis.fetch;
    if (typeof fetcher !== "function") {
      throw new TrazaError("no fetch is available; pass one as the `fetch` option");
    }
    try {
      let response: Response;
      try {
        response = await fetcher(url, { method, headers, body, signal: deadline.signal });
      } catch (error) {
        throw this.#transportError(error, options.signal, deadline.timedOut(), timeoutMs);
      }
      let text: string;
      try {
        text = await response.text();
      } catch (error) {
        throw this.#transportError(error, options.signal, deadline.timedOut(), timeoutMs);
      }
      if (!response.ok) {
        const names: Record<string, string> = {};
        response.headers.forEach((value, name) => {
          names[name.toLowerCase()] = value;
        });
        throw errorFromResponse(response.status, response.statusText, names, text);
      }
      return text;
    } finally {
      deadline.dispose();
    }
  }

  #transportError(error: unknown, userSignal: AbortSignal | undefined, timedOut: boolean, timeoutMs: number): unknown {
    // The caller's own cancellation propagates as it came (an AbortError,
    // or the reason they passed): it is their decision, not a failure.
    if (userSignal?.aborted) return userSignal.reason ?? error;
    if (timedOut) return new TrazaError(`no answer from ${this.url} within ${timeoutMs} ms`, { cause: error });
    // Node's fetch says only "fetch failed"; the reason (ECONNREFUSED,
    // ENOTFOUND, …) is on its cause, and it is what an operator needs.
    const detail = error instanceof Error ? error.message : String(error);
    const code = (error as { cause?: { code?: unknown } } | null)?.cause?.code;
    const reason = typeof code === "string" ? ` (${code})` : "";
    return new TrazaError(`could not reach ${this.url}: ${detail}${reason}`, { cause: error });
  }

  #tenantQuery(): { tenant?: string } {
    return this.tenant === undefined ? {} : { tenant: this.tenant };
  }
}

/** A replay created by {@link Client.replay}. */
export class Replay implements ReplayDocument {
  readonly id: string;
  readonly run: string;
  readonly mode: "strict" | "loose";
  readonly steps: number;
  readonly llm_steps: number;
  readonly expires_at_ns: bigint;
  readonly unreadable_tool_steps: number[];
  readonly base_urls: { openai: string; anthropic: string };
  readonly #client: Client;

  constructor(client: Client, document: ReplayDocument) {
    this.#client = client;
    this.id = document.id;
    this.run = document.run;
    this.mode = document.mode;
    this.steps = document.steps;
    this.llm_steps = document.llm_steps;
    this.expires_at_ns = document.expires_at_ns;
    this.unreadable_tool_steps = document.unreadable_tool_steps;
    this.base_urls = document.base_urls;
  }

  // Concatenated rather than resolved with `new URL(path, base)`: the
  // server's paths are absolute (/v1/...), and resolving would drop a path
  // prefix the client's URL carries when Traza sits behind a reverse proxy.

  /** The absolute base URL for the OpenAI SDK (`…/openai/v1`). */
  get openaiBaseUrl(): string {
    return this.#client.url + this.base_urls.openai;
  }

  /** The absolute base URL for the Anthropic SDK (`…/anthropic`). */
  get anthropicBaseUrl(): string {
    return this.#client.url + this.base_urls.anthropic;
  }

  /**
   * The environment that points both providers' SDKs at this replay. The
   * keys are the client's Traza token (the OpenAI SDK sends it as a bearer
   * token, the Anthropic SDK as `x-api-key`, which the replay routes accept);
   * on an open server any non-empty key works and the SDKs refuse to start
   * without one, so a placeholder stands in.
   */
  env(): ReplayEnv {
    const key = this.#client.token ?? "traza-open";
    return {
      OPENAI_BASE_URL: this.openaiBaseUrl,
      OPENAI_API_KEY: key,
      ANTHROPIC_BASE_URL: this.anthropicBaseUrl,
      ANTHROPIC_API_KEY: key,
    };
  }

  /** What the replay has served so far, and its first divergence. */
  status(options: CallOptions = {}): Promise<ReplayStatus> {
    return this.#client.request<ReplayStatus>("GET", `/v1/replays/${segment(this.id)}`, options);
  }

  /**
   * The next recorded tool step not yet served (the next named `tool`, if
   * given), or `null` when none is left — so a harness can loop until it
   * runs out. A step whose value cannot be read throws {@link Conflict}
   * (`type: "tool_unreadable"`) and is not consumed.
   */
  async nextTool(tool?: string, options: CallOptions = {}): Promise<ToolStep | null> {
    try {
      return await this.#client.request<ToolStep>("POST", `/v1/replays/${segment(this.id)}/tools/next`, {
        signal: options.signal,
        body: tool === undefined ? {} : { tool },
      });
    } catch (error) {
      // Running out is how a harness's loop ends, not a failure; a replay an
      // erasure withdrew (also 410) still throws.
      if (error instanceof TrazaError && error.status === 410 && error.type === "replay_exhausted") return null;
      throw error;
    }
  }
}

/** The async iterable {@link Client.events} returns. */
export class EventStream implements AsyncIterable<FuseEvent> {
  /** Just past the last event handed out; pass it back as `after` to resume. */
  cursor: string | undefined;
  /** True once the server answered that it could not honour a cursor. */
  gap = false;
  readonly #client: Client;
  readonly #waitMs: number;
  readonly #follow: boolean;
  readonly #signal: AbortSignal | undefined;

  constructor(client: Client, options: EventOptions) {
    const waitMs = options.waitMs ?? 0;
    if (!(waitMs >= 0 && waitMs <= 30_000)) throw new RangeError("waitMs must be between 0 and 30000");
    this.#client = client;
    this.cursor = options.after;
    this.#waitMs = waitMs;
    this.#follow = options.follow ?? false;
    this.#signal = options.signal;
  }

  async *[Symbol.asyncIterator](): AsyncGenerator<FuseEvent, void, undefined> {
    for (;;) {
      const page = await this.#client.request<{ events: FuseEvent[]; next?: string; gap?: boolean }>(
        "GET",
        "/v1/fuses/events",
        {
          signal: this.#signal,
          query: {
            tenant: this.#client.tenant,
            wait_ms: this.#waitMs,
            after: this.cursor,
          },
          // The server holds a long-poll open for up to waitMs; the request
          // timeout must outlast it or every quiet poll would read as an
          // unreachable server.
          timeoutMs: this.#client.timeoutMs + this.#waitMs,
        },
      );
      if (page.gap) this.gap = true;
      for (const event of page.events) {
        // Advanced before the yield, so a consumer that breaks out early
        // resumes just past the last event it was handed.
        this.cursor = event.cursor;
        yield event;
      }
      // `next` can be past the last event: it also skips events a tenant
      // filter hid, so a filtered reader keeps pace.
      if (page.next !== undefined) this.cursor = page.next;
      if (page.events.length === 0 && !this.#follow) return;
    }
  }
}

/** Percent-encodes one path segment: a run id may contain `/`. */
function segment(value: string): string {
  if (typeof value !== "string" || value === "") throw new TypeError("expected a non-empty string");
  return encodeURIComponent(value);
}

/**
 * One signal that aborts on the caller's signal or after `timeoutMs`,
 * whichever comes first. Built by hand rather than with `AbortSignal.any` and
 * `AbortSignal.timeout`, which Node 20.0–20.2 and older browsers lack.
 */
function linkSignals(
  user: AbortSignal | undefined,
  timeoutMs: number,
): { signal: AbortSignal; timedOut: () => boolean; dispose: () => void } {
  const controller = new AbortController();
  let timedOut = false;
  const timer =
    timeoutMs > 0
      ? setTimeout(() => {
          timedOut = true;
          controller.abort(new TrazaError(`timed out after ${timeoutMs} ms`));
        }, timeoutMs)
      : undefined;
  const onAbort = () => controller.abort(user?.reason);
  if (user) {
    if (user.aborted) controller.abort(user.reason);
    else user.addEventListener("abort", onAbort, { once: true });
  }
  return {
    signal: controller.signal,
    timedOut: () => timedOut,
    dispose: () => {
      if (timer !== undefined) clearTimeout(timer);
      user?.removeEventListener("abort", onAbort);
    },
  };
}
