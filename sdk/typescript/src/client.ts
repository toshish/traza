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
import { Fork, type ForkOptions } from "./fork.js";
import { parseJson, stringifyJson } from "./json.js";
import type {
  Diff,
  EvidencePack,
  ForkEdit,
  ForkInfo,
  Forks,
  FuseEvent,
  Goldens,
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
  /**
   * Sent as `Authorization: Bearer`; leave it out for an open server. A
   * redirect is never followed, so the token goes only to the client's URL.
   */
  token?: string;
  /**
   * Scopes every call that takes a tenant, for an unbound credential. A bound
   * credential can leave it out (the server scopes it to its own tenant) or
   * name its own tenant; naming any other, `""` included, is refused with
   * {@link AuthError}. `""` names the default tenant.
   */
  tenant?: string;
  /** Per request, in milliseconds. Default 10000. A long-poll adds its wait. */
  timeoutMs?: number;
  /**
   * A `fetch` to use instead of the global one. It is called with
   * `redirect: "manual"`; one that follows redirects regardless would carry
   * the token wherever they point.
   */
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
   * When the check cannot be answered (an unreachable server, a refused
   * check, or an answer that is not a verdict, such as a proxy's HTML page),
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
  /**
   * How long each read waits for an event when none is there, 0–30000 ms.
   * Defaults to 0 (drain and end), or to {@link FOLLOW_WAIT_MS} when
   * following; `follow` with `waitMs: 0` throws a `RangeError`.
   */
  waitMs?: number;
  /** Keep reading forever, each read a long-poll, instead of ending at the first empty read. */
  follow?: boolean;
}

/**
 * How long each read of a following event stream is held open when the
 * caller names no wait. Under the server's 30 s ceiling, and under the 30 to
 * 60 s idle timeouts common in reverse proxies and load balancers, so a quiet
 * read is answered by Traza rather than cut off by something in between.
 */
export const FOLLOW_WAIT_MS = 25_000;

const VERDICTS: readonly unknown[] = ["continue", "warn", "stop"];

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

// Replay and EventStream read through Client's private transport. A static
// block is the one place outside Client's own methods that can reach a
// private member, so it hands this module (and nothing it exports) a way in.
let readObject: <T>(client: Client, method: string, path: string, options: RequestOptions) => Promise<T>;

/** A client for one Traza server. */
export class Client {
  static {
    readObject = (client, method, path, options) => client.#object(method, path, options);
  }

  /** The server's base URL, without a trailing slash. */
  readonly url: string;
  readonly tenant: string | undefined;
  readonly timeoutMs: number;
  // A private field rather than a property: console.log(client) and
  // JSON.stringify(client) must never print the token.
  readonly #token: string | undefined;
  readonly #fetch: FetchLike | undefined;
  // Scheme, host and port: the only place the token may be sent.
  readonly #origin: string;

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
    this.#origin = parsed.origin;
    this.#token = options.token || undefined;
    this.tenant = options.tenant;
    this.timeoutMs = options.timeoutMs ?? 10_000;
    this.#fetch = options.fetch;
  }

  /**
   * A client from `TRAZA_URL`, `TRAZA_TOKEN` and `TRAZA_TENANT` — the
   * variables the Claude Code hook in `examples/guard` reads, read the way it
   * reads them: a variable set to the empty string is unset, so
   * `TRAZA_TENANT=` left in a profile does not name the default tenant, which
   * a bound credential would be refused. Defaults to `process.env` where
   * there is one.
   */
  static fromEnv(env?: Record<string, string | undefined>): Client {
    const source =
      env ?? (globalThis as { process?: { env?: Record<string, string | undefined> } }).process?.env ?? {};
    return new Client(source.TRAZA_URL || DEFAULT_URL, {
      token: source.TRAZA_TOKEN || undefined,
      tenant: source.TRAZA_TENANT || undefined,
    });
  }

  /** The bearer token, if any. */
  get token(): string | undefined {
    return this.#token;
  }

  // ---------------------------------------------------------- identity

  /** What the calling credential is: scope, binding, operator or not. */
  whoami(options: CallOptions = {}): Promise<Identity> {
    return this.#object<Identity>("GET", "/v1/whoami", options);
  }

  /**
   * Ingest counters, limits and quota state, per tenant. A bound credential
   * gets its own row; an unbound one every tenant, or the client's tenant.
   */
  usage(options: CallOptions = {}): Promise<UsageReport> {
    return this.#object<UsageReport>("GET", "/v1/usage", { ...options, query: this.#tenantQuery() });
  }

  // ---------------------------------------------------------- fuses

  /**
   * The pre-call gate: may `run` take its next step? The step — an LLM call,
   * or a call of `tool` — is added to the run's recorded usage
   * hypothetically, with the estimates; a step that would go past a `stop`
   * limit answers `stop`. Nothing about the run changes, and a `stop` is a
   * normal answer here: {@link guard} is the form that throws. An answer that
   * names no verdict (`continue`, `warn` or `stop`) rejects with
   * {@link TrazaError} rather than passing for `continue`.
   */
  check(run: string, options: CheckOptions = {}): Promise<Verdict> {
    const next: Record<string, unknown> = {};
    if (options.estCostUsd !== undefined) next.est_cost_usd = options.estCostUsd;
    if (options.estTokens !== undefined) next.est_tokens = options.estTokens;
    if (options.tool !== undefined) next.tool = options.tool;
    const body: Record<string, unknown> = { run, next };
    if (this.tenant !== undefined) body.tenant = this.tenant;
    return this.#object<Verdict>("POST", "/v1/fuses/check", { signal: options.signal, body }, true);
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
    return this.#object<Verdict>("GET", `/v1/fuses/runs/${segment(run)}`, {
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
    const page = await this.#object<{ runs: Verdict[] }>("GET", "/v1/fuses/runs", {
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
    return this.#object<Verdict>("POST", `/v1/fuses/runs/${segment(run)}/trip`, {
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
    return this.#object<Verdict>("POST", `/v1/fuses/runs/${segment(run)}/reset`, {
      signal: options.signal,
      query: this.#tenantQuery(),
      body: options.grant === undefined ? {} : { grant: options.grant },
    });
  }

  /** The stored fuse policies and the server's global default. */
  policies(options: CallOptions = {}): Promise<Policies> {
    return this.#object<Policies>("GET", "/v1/fuses/policies", { ...options, query: this.#tenantQuery() });
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
    return this.#object<Policy>("POST", "/v1/fuses/policies", { signal: options.signal, body });
  }

  /** Deletes a policy by id; resolves the policy as it was. */
  async deletePolicy(id: string, options: CallOptions = {}): Promise<Policy> {
    const answer = await this.#object<{ deleted: boolean; policy: Policy }>(
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
    return this.#object<Tape>("GET", `/v1/tapes/${segment(run)}`, {
      signal: options.signal,
      query: { ...this.#tenantQuery(), resolve: options.resolve ? "true" : undefined, max_steps: options.maxSteps },
    });
  }

  /**
   * Snapshots `run` into a replay that serves its model answers back.
   * `strict` (the default) refuses a request whose messages differ from the
   * recorded step (the refusal names the first difference); `loose` serves
   * in order regardless. It lives in server memory for an hour.
   *
   * `edits` and `liveAfter` make it a fork (see {@link fork}, which also
   * switches the agent to the real provider at the cut): each edit is served
   * instead of the recording at its step, and after step `liveAfter` every
   * call answers `409 replay_live`.
   */
  async replay(
    run: string,
    options: CallOptions & { mode?: "strict" | "loose"; edits?: ForkEdit[]; liveAfter?: number } = {},
  ): Promise<Replay> {
    const body: Record<string, unknown> = { run, mode: options.mode ?? "strict" };
    if (options.edits?.length) body.edits = options.edits;
    if (options.liveAfter !== undefined) body.live_after = options.liveAfter;
    if (this.tenant !== undefined) body.tenant = this.tenant;
    const document = await this.#object<ReplayDocument>("POST", "/v1/replays", { signal: options.signal, body });
    return new Replay(this, document);
  }

  /**
   * Branches `run` at tape step `atStep`. Steps before it are replayed from
   * the recording, strictly; at it the agent receives `response` (a model
   * answer) or `toolResult` (from {@link Fork.toolResult}) instead — or, with
   * neither, the recording stops there; with `live` (the default) every
   * later call goes to the real provider through the fork's switching
   * `fetch`. See {@link Fork}.
   */
  async fork(run: string, options: ForkOptions): Promise<Fork> {
    const { edits, liveAfter } = Fork.spec(options);
    const replay = await this.replay(run, { mode: options.mode, edits, liveAfter, signal: options.signal });
    return new Fork(this, replay, run, options);
  }

  /** The recorded forks of `run`, newest first, each diffed against it. */
  forks(run: string, options: CallOptions & { limit?: number } = {}): Promise<Forks> {
    return this.#object<Forks>("GET", "/v1/forks", {
      signal: options.signal,
      query: { ...this.#tenantQuery(), run, limit: options.limit },
    });
  }

  /** The runs marked golden, by span attribute or by annotation. */
  goldens(options: CallOptions & { limit?: number } = {}): Promise<Goldens> {
    return this.#object<Goldens>("GET", "/v1/goldens", {
      signal: options.signal,
      query: { ...this.#tenantQuery(), limit: options.limit },
    });
  }

  /**
   * Marks `run` as a regression baseline — or with `golden: false` un-marks
   * it — by an annotation named `traza.golden` on its session (or, for
   * `trace:<id>`, its trace). The newest one decides. Needs an `rw` credential.
   */
  async markGolden(run: string, options: CallOptions & { golden?: boolean; source?: string } = {}): Promise<void> {
    const body: Record<string, unknown> = {
      name: "traza.golden",
      value: options.golden ?? true,
      source: options.source ?? "human:traza-sdk",
    };
    if (run.startsWith("trace:")) body.trace_id = run.slice("trace:".length);
    else body.session_id = segmentCheck(run);
    if (this.tenant !== undefined) body.tenant = this.tenant;
    await this.#object("POST", "/v1/annotations", { signal: options.signal, body });
  }

  /** Aligns two runs' steps and names the first place they diverge. */
  diff(a: string, b: string, options: CallOptions = {}): Promise<Diff> {
    return this.#object<Diff>("GET", "/v1/diff", { signal: options.signal, query: { ...this.#tenantQuery(), a, b } });
  }

  /**
   * Earlier runs whose recorded text holds every word of `query`, successes
   * first under `outcome: "any"`.
   */
  recall(
    query: string,
    options: CallOptions & { outcome?: "any" | "success" | "failure"; limit?: number } = {},
  ): Promise<Recall> {
    return this.#object<Recall>("GET", "/v1/recall", {
      signal: options.signal,
      query: { ...this.#tenantQuery(), q: query, outcome: options.outcome ?? "any", limit: options.limit ?? 5 },
    });
  }

  // ---------------------------------------------------------- ledger

  /** The ledger's tip: what to anchor with an external notary. */
  ledgerHead(options: CallOptions = {}): Promise<LedgerHead> {
    return this.#object<LedgerHead>("GET", "/v1/ledger/head", options);
  }

  /**
   * The evidence pack for a window of Unix nanoseconds. A bound credential
   * gets its tenant's pack; an unbound one the client's tenant, or the whole
   * store. `verifyDigest()` on the result recomputes its digest.
   */
  async evidence(
    options: CallOptions & { since?: bigint | number; until?: bigint | number } = {},
  ): Promise<EvidencePack> {
    const { status, text } = await this.#send("GET", "/v1/evidence", {
      signal: options.signal,
      query: { ...this.#tenantQuery(), since: options.since, until: options.until },
    });
    const pack = this.#asObject(status, text) as unknown as EvidencePack;
    Object.defineProperty(pack, "verifyDigest", { value: () => verifyEvidenceDigest(text), enumerable: false });
    return pack;
  }

  // ---------------------------------------------------------- transport

  /**
   * Sends one request and resolves its parsed JSON body: the escape hatch for
   * routes this client does not wrap, with the same credential, timeout and
   * error mapping as every method above. `path` is appended to the client's
   * URL and must start with `/`: any other path would continue the URL's host
   * (`.attacker.example` names another server), so it rejects with a
   * `TypeError` before anything is sent, as does a request URL whose scheme,
   * host or port is not the client's. A success whose body is not JSON
   * rejects with {@link TrazaError}.
   */
  async request<T = unknown>(method: string, path: string, options: RequestOptions = {}): Promise<T> {
    const { status, text } = await this.#send(method, path, options);
    return this.#parse(status, text) as T;
  }

  /**
   * {@link request} for a route that answers a JSON object. With `verdict`
   * the object must also name a verdict the guard knows: an answer it cannot
   * act on must not read as `continue`.
   */
  async #object<T>(method: string, path: string, options: RequestOptions, verdict = false): Promise<T> {
    const { status, text } = await this.#send(method, path, options);
    return this.#asObject(status, text, verdict) as T;
  }

  #asObject(status: number, text: string, verdict = false): Record<string, unknown> {
    const value = this.#parse(status, text);
    if (value === null || typeof value !== "object" || Array.isArray(value)) {
      throw this.#unreadable(status, text, `expected a JSON object, got ${jsonKind(value)}`);
    }
    const object = value as Record<string, unknown>;
    if (verdict && !VERDICTS.includes(object.verdict)) {
      throw this.#unreadable(status, text, "it names no verdict (continue, warn or stop)");
    }
    return object;
  }

  #parse(status: number, text: string): unknown {
    if (!text) return null;
    try {
      return parseJson(text);
    } catch (error) {
      throw this.#unreadable(status, text, "not JSON", error);
    }
  }

  #unreadable(status: number, body: string, why: string, cause?: unknown): TrazaError {
    // A reverse proxy's sign-in or maintenance page, a captive portal: an
    // answer arrived, but not one Traza gave. It is a TrazaError like a
    // refused or unreachable request, so a fail-open guard and code that
    // rides out an unavailable server treat it alike instead of meeting an
    // error they never expected.
    return new TrazaError(`unreadable answer from ${this.url}: ${why}`, { status, body, cause });
  }

  async #send(method: string, path: string, options: RequestOptions): Promise<{ status: number; text: string }> {
    // Appended, not resolved, so a reverse proxy's path prefix on the
    // client's URL is kept. That makes a path not starting with "/" a
    // continuation of the host: "https://traza.example" and
    // ".attacker.example/v1/whoami" name another host, which the token would
    // reach with no redirect involved. Both checks come before the token is
    // attached; the second holds whatever builds the path.
    if (typeof path !== "string" || !path.startsWith("/")) {
      throw new TypeError(`path must start with "/", got ${JSON.stringify(path)}`);
    }
    let url = this.url + path;
    const query = new URLSearchParams();
    for (const [key, value] of Object.entries(options.query ?? {})) {
      if (value !== undefined && value !== null) query.append(key, String(value));
    }
    const search = query.toString();
    if (search) url += `?${search}`;
    // The parsed URL is what is fetched, so a `fetch` of the caller's that
    // parses differently still gets the URL whose origin was checked.
    let target: URL | undefined;
    try {
      target = new URL(url);
    } catch {
      // Not a URL at all: refused below, like one on another origin.
    }
    if (target?.origin !== this.#origin) {
      throw new TypeError(`${JSON.stringify(url)} is not a URL on the client's origin ${this.#origin}`);
    }

    const headers: Record<string, string> = { accept: "application/json" };
    let body: string | undefined;
    if (options.body !== undefined) {
      headers["content-type"] = "application/json";
      body = stringifyJson(options.body);
    }
    if (this.#token) headers.authorization = `Bearer ${this.#token}`;

    const fetcher = this.#fetch ?? globalThis.fetch;
    if (typeof fetcher !== "function") {
      throw new TrazaError("no fetch is available; pass one as the `fetch` option");
    }
    const timeoutMs = options.timeoutMs ?? this.timeoutMs;
    // Armed only now, with the try below to disarm it: a timer started
    // before a throw would hold the event loop open until it fired.
    const deadline = linkSignals(options.signal, timeoutMs);
    try {
      let response: Response;
      try {
        // "manual": Traza never redirects its own routes, so a 3xx comes from
        // something in front of it (a proxy canonicalising a host or
        // upgrading a scheme). Following it would carry the token along in a
        // same-origin hop, and in any fetch that does not strip it across
        // origins as the platform's does.
        response = await fetcher(target.href, { method, headers, body, signal: deadline.signal, redirect: "manual" });
      } catch (error) {
        throw this.#transportError(error, options.signal, deadline.timedOut(), timeoutMs);
      }
      let text: string;
      try {
        text = await response.text();
      } catch (error) {
        throw this.#transportError(error, options.signal, deadline.timedOut(), timeoutMs);
      }
      const names: Record<string, string> = {};
      response.headers.forEach((value, name) => {
        names[name.toLowerCase()] = value;
      });
      // A browser hides a manual redirect behind an opaque response with
      // status 0 and no headers; elsewhere it is the 3xx itself.
      if (response.type === "opaqueredirect" || (response.status >= 300 && response.status < 400)) {
        const location = names.location ?? "an unnamed location";
        const status = response.status ? `${response.status}, ` : "";
        throw new TrazaError(
          `${this.url} answered ${status}a redirect to ${location}; the client follows no redirect, ` +
            "so its token goes only to the URL it was given: configure the final URL",
          { status: response.status || null, headers: names },
        );
      }
      if (!response.ok) throw errorFromResponse(response.status, response.statusText, names, text);
      return { status: response.status, text };
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
  /** Present (true) when `steps` counts only the run's beginning. */
  readonly steps_truncated?: boolean;
  readonly llm_steps: number;
  readonly expires_at_ns: bigint;
  readonly unreadable_tool_steps: number[];
  readonly base_urls: { openai: string; anthropic: string };
  /** Present when the replay is a fork. */
  readonly fork_id?: string;
  readonly fork?: ForkInfo;
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
    if (document.steps_truncated !== undefined) this.steps_truncated = document.steps_truncated;
    if (document.fork_id !== undefined) this.fork_id = document.fork_id;
    if (document.fork !== undefined) this.fork = document.fork;
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
   * keys are `apiKey` when given, else the client's Traza token (the OpenAI
   * SDK sends it as a bearer token, the Anthropic SDK as `x-api-key`, which
   * the replay routes accept); on an open server any non-empty key works and
   * the SDKs refuse to start without one, so a placeholder stands in.
   *
   * Whatever key this returns, every library in the process that gets the
   * environment can read it, and one that reads `OPENAI_API_KEY` but ignores
   * `OPENAI_BASE_URL` sends it to the real provider. When the client holds an
   * operator's token, pass a dedicated `ro` credential bound to the run's
   * tenant as `apiKey`: any valid credential of the replay's tenant may
   * consume it.
   */
  env(options: { apiKey?: string } = {}): ReplayEnv {
    const key = options.apiKey || this.#client.token || "traza-open";
    return {
      OPENAI_BASE_URL: this.openaiBaseUrl,
      OPENAI_API_KEY: key,
      ANTHROPIC_BASE_URL: this.anthropicBaseUrl,
      ANTHROPIC_API_KEY: key,
    };
  }

  /** What the replay has served so far, and its first divergence. */
  status(options: CallOptions = {}): Promise<ReplayStatus> {
    return readObject<ReplayStatus>(this.#client, "GET", `/v1/replays/${segment(this.id)}`, options);
  }

  /**
   * The next recorded tool step not yet served (the next named `tool`, if
   * given), or `null` when none is left — so a harness can loop until it
   * runs out. A step whose value cannot be read throws {@link Conflict}
   * (`type: "tool_unreadable"`) and is not consumed.
   */
  async nextTool(tool?: string, options: CallOptions = {}): Promise<ToolStep | null> {
    try {
      return await readObject<ToolStep>(this.#client, "POST", `/v1/replays/${segment(this.id)}/tools/next`, {
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
    const follow = options.follow ?? false;
    const waitMs = options.waitMs ?? (follow ? FOLLOW_WAIT_MS : 0);
    if (!(waitMs >= 0 && waitMs <= 30_000)) throw new RangeError("waitMs must be between 0 and 30000");
    // Every empty read would be followed at once by the next one: a request
    // loop as fast as the network allows.
    if (follow && waitMs === 0) {
      throw new RangeError("follow needs a long-poll: give waitMs > 0, or leave it out for 25000");
    }
    this.#client = client;
    this.cursor = options.after;
    this.#waitMs = waitMs;
    this.#follow = follow;
    this.#signal = options.signal;
  }

  async *[Symbol.asyncIterator](): AsyncGenerator<FuseEvent, void, undefined> {
    for (;;) {
      const page = await readObject<{ events?: FuseEvent[]; next?: string; gap?: boolean }>(
        this.#client,
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
      const events = page.events ?? [];
      for (const event of events) {
        // Advanced before the yield, so a consumer that breaks out early
        // resumes just past the last event it was handed.
        this.cursor = event.cursor;
        yield event;
      }
      // `next` can be past the last event: it also skips events a tenant
      // filter hid, so a filtered reader keeps pace.
      if (page.next !== undefined) this.cursor = page.next;
      if (events.length === 0 && !this.#follow) return;
    }
  }
}

function jsonKind(value: unknown): string {
  if (value === null) return "nothing";
  if (Array.isArray(value)) return "an array";
  return `a ${typeof value === "bigint" ? "number" : typeof value}`;
}

/** Percent-encodes one path segment: a run id may contain `/`. */
function segment(value: string): string {
  return encodeURIComponent(segmentCheck(value));
}

function segmentCheck(value: string): string {
  if (typeof value !== "string" || value === "") throw new TypeError("expected a non-empty string");
  return value;
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
