/**
 * Fork & Rewind: branch a recorded run at any step, and let the agent go on.
 *
 * A fork replays a recorded run up to a step, serves a different answer
 * there (or simply stops serving the recording), and hands every later call
 * to the real provider — so the agent continues, for real, down a branch the
 * recording never took:
 *
 * ```ts
 * const fork = await traza.fork("weather-7", { atStep: 2, response: "It is sunny in Paris." });
 * const openai = new OpenAI(fork.openaiOptions());   // baseURL, apiKey and the switching fetch
 * await runAgent(openai);                            // steps 0-1 replayed, 2 edited, then live
 * fork.close();
 * ```
 *
 * The switch is the `fetch` the fork hands the SDK: it sends each call to
 * the Traza replay until the replay answers `409 replay_live`, then sends
 * that call and every later one to the real provider, with the provider's
 * key — the base URL and key the environment named when the fork was made,
 * or `live` options. The SDK sees one provider throughout. Only a model
 * call's answer moves the switch: the server keeps the tool and model
 * cursors apart, so a tool the recording does not hold
 * ({@link Fork.toolResult} answering `null`) runs for real while the
 * recorded model calls are still served from the recording. Traza itself
 * never calls a model: the live half is the agent's own traffic, from the
 * agent's own process.
 *
 * Spans: {@link Fork.spanProcessor} returns an OpenTelemetry span processor
 * (duck-typed, so this package needs no dependency) that stamps the fork's
 * tags — `traza.fork.parent`, `traza.fork.step`, `traza.fork.id` — and its
 * session id on the spans started in the fork's OpenTelemetry context. That
 * is what lets `GET /v1/forks` find the branch again. Run the agent in that
 * context, so a server handling other requests meanwhile keeps their spans
 * in their own runs:
 *
 * ```ts
 * import { context } from "@opentelemetry/api";
 * provider.addSpanProcessor(fork.spanProcessor());
 * await fork.run(context, () => runAgent(openai));
 * ```
 *
 * @module
 */

import type { CallOptions, Client, FetchLike, Replay } from "./client.js";
import { TrazaError } from "./errors.js";
import type { Diff, ForkEdit, ReplayStatus, ToolStep } from "./types.js";

/** The span attribute naming the run a forked run branched from. */
export const FORK_PARENT = "traza.fork.parent";
/** The span attribute naming the step the fork was cut at. */
export const FORK_STEP = "traza.fork.step";
/** The span attribute carrying the fork's id. */
export const FORK_ID = "traza.fork.id";

/**
 * The OpenTelemetry context key a fork is carried under. `Symbol.for`, which
 * is what `createContextKey("traza.fork")` in `@opentelemetry/api` returns,
 * so a harness may set it with either.
 */
export const FORK_CONTEXT_KEY: symbol = Symbol.for("traza.fork");

const DEFAULT_OPENAI_BASE_URL = "https://api.openai.com/v1";
const DEFAULT_ANTHROPIC_BASE_URL = "https://api.anthropic.com";

/** Where a fork's live calls go. */
export interface LiveProvider {
  /** Default: `OPENAI_BASE_URL` / `ANTHROPIC_BASE_URL`, else the provider's API. */
  baseURL?: string;
  /** Default: `OPENAI_API_KEY` / `ANTHROPIC_API_KEY`. */
  apiKey?: string;
}

/** Options for {@link Client.fork}. */
export interface ForkOptions extends CallOptions {
  /** The tape step to cut at: the first step whose answer is not the recording's. */
  atStep: number;
  /** A model answer served at `atStep`: text, an assistant message, or a list of them. */
  response?: unknown;
  /** A tool result served at `atStep` by {@link Fork.toolResult}; `null` is a result. */
  toolResult?: unknown;
  /**
   * Send every call after the cut to the real provider (default). With
   * `false`, an edit is served and the recording then plays on, loosely.
   */
  live?: boolean;
  mode?: "strict" | "loose";
  /** The session id the forked run's spans get (default: the fork id). */
  runId?: string;
  /** Stamp `session.id` on the fork's spans (default true). */
  tagSession?: boolean;
  liveOpenAI?: LiveProvider;
  liveAnthropic?: LiveProvider;
  /** The transport for replay and live calls alike; default the global `fetch`. */
  fetch?: FetchLike;
}

/** The parts of an OpenTelemetry span the fork's processor touches. */
export interface TaggableSpan {
  setAttribute(key: string, value: string | number): unknown;
}

/** The parts of an OpenTelemetry `Context` a fork reads and writes. */
export interface OtelContext {
  getValue(key: symbol): unknown;
  setValue(key: symbol, value: unknown): OtelContext;
}

/** The parts of the OpenTelemetry context API (`context` from `@opentelemetry/api`) {@link Fork.run} uses. */
export interface OtelContextApi<C extends OtelContext = OtelContext> {
  active(): C;
  with<T>(context: C, fn: () => T): T;
}

/** Options for {@link Fork.spanProcessor}. */
export interface SpanProcessorOptions {
  /**
   * Tag every span started while the fork is open, whatever its context
   * (default false). Only for a process that runs nothing but the forked
   * agent: any other work's spans would join the fork's run too.
   */
  allSpans?: boolean;
}

/** A duck-typed OpenTelemetry `SpanProcessor`. */
export interface ForkSpanProcessor {
  onStart(span: TaggableSpan, parentContext?: unknown): void;
  onEnd(span: unknown): void;
  shutdown(): Promise<void>;
  forceFlush(): Promise<void>;
}

type FetchInput = string | URL | Request;

/** A branch of a recorded run, made by {@link Client.fork}. */
export class Fork {
  /** The run it branches from. */
  readonly parent: string;
  /** The step asked for. */
  readonly atStep: number;
  /** The replay serving the fork's beginning. */
  readonly replay: Replay;
  /** The fork id, `fk_…`. */
  readonly id: string;
  /** The step it was cut at, as the server computed it. */
  readonly step: number;
  /** Whether calls go live after the cut. */
  readonly live: boolean;
  /**
   * When model calls went to the real provider: when the replay first
   * answered a model call `replay_live`. A tool step answered that way
   * ({@link toolResult} returning `null`) does not set it — tool and model
   * calls have their own cursors.
   */
  liveAt: Date | null = null;
  /**
   * The switching `fetch`: pass it to the OpenAI or Anthropic SDK (both take
   * a `fetch` option), or use {@link openaiOptions} / {@link anthropicOptions}.
   */
  readonly fetch: (input: FetchInput, init?: RequestInit) => Promise<Response>;
  readonly #client: Client;
  readonly #runId: string | undefined;
  readonly #tagSession: boolean;
  readonly #openai: { baseURL: string; apiKey: string | undefined };
  readonly #anthropic: { baseURL: string; apiKey: string | undefined };
  readonly #transport: FetchLike;
  #closed = false;

  /** @internal Use {@link Client.fork}. */
  constructor(client: Client, replay: Replay, run: string, options: ForkOptions) {
    if (!replay.fork_id || !replay.fork) {
      throw new TrazaError("the server returned no fork for this replay; is it older than this client?");
    }
    this.#client = client;
    this.replay = replay;
    this.parent = run;
    this.atStep = options.atStep;
    this.id = replay.fork_id;
    this.step = replay.fork.step;
    this.live = options.live ?? true;
    this.#runId = options.runId;
    this.#tagSession = options.tagSession ?? true;
    const env = processEnv();
    this.#openai = {
      baseURL: trimSlash(options.liveOpenAI?.baseURL || env.OPENAI_BASE_URL || DEFAULT_OPENAI_BASE_URL),
      apiKey: options.liveOpenAI?.apiKey || env.OPENAI_API_KEY || undefined,
    };
    this.#anthropic = {
      baseURL: trimSlash(options.liveAnthropic?.baseURL || env.ANTHROPIC_BASE_URL || DEFAULT_ANTHROPIC_BASE_URL),
      apiKey: options.liveAnthropic?.apiKey || env.ANTHROPIC_API_KEY || undefined,
    };
    const transport = options.fetch ?? (globalThis.fetch as FetchLike | undefined);
    if (typeof transport !== "function") throw new TrazaError("no fetch is available; pass one as the `fetch` option");
    this.#transport = transport;
    this.fetch = (input, init) => this.#switch(input, init ?? {});
  }

  /** The `edits` and `live_after` a fork asks `POST /v1/replays` for. */
  static spec(options: ForkOptions): { edits: ForkEdit[]; liveAfter: number | undefined } {
    const { atStep } = options;
    if (!Number.isInteger(atStep) || atStep < 0) {
      throw new RangeError("atStep must be a non-negative integer: a step of the run's tape");
    }
    const hasResponse = options.response !== undefined && options.response !== null;
    const hasToolResult = "toolResult" in options && options.toolResult !== undefined;
    if (hasResponse && hasToolResult) throw new TypeError("an edit is a response or a toolResult, not both");
    const edits: ForkEdit[] = hasResponse
      ? [{ step: atStep, response: options.response }]
      : hasToolResult
        ? [{ step: atStep, tool_result: options.toolResult }]
        : [];
    const live = options.live ?? true;
    if (edits.length === 0) {
      if (!live) throw new TypeError("a fork needs an edit (response or toolResult), or live: true");
      if (atStep === 0) {
        throw new RangeError(
          "atStep 0 with no edit replays nothing: the run would be live from its first call, which is just running the agent",
        );
      }
    }
    const liveAfter = !live ? undefined : edits.length ? atStep : atStep - 1;
    return { edits, liveAfter };
  }

  /**
   * Whether model calls now go to the real provider: the replay has answered
   * one `replay_live`. Only the model endpoint's answer says so — a tool the
   * recording lacks going live leaves the recorded model calls on the
   * recording.
   */
  get isLive(): boolean {
    return this.liveAt !== null;
  }

  /** The session id the forked run records under: `runId`, else the fork id. */
  get runId(): string {
    return this.#runId ?? this.id;
  }

  /** The fork's tags, as the server returned them. */
  get attributes(): Record<string, string | number> {
    return { ...(this.replay.fork?.attributes ?? { [FORK_PARENT]: this.parent, [FORK_STEP]: this.step, [FORK_ID]: this.id }) };
  }

  /** What the span processor stamps: the tags, and the session id unless `tagSession: false`. */
  spanAttributes(): Record<string, string | number> {
    const attributes = this.attributes;
    if (this.#tagSession) attributes["session.id"] = this.runId;
    return attributes;
  }

  /** Options for `new OpenAI(...)`: the replay's base URL, the client's token and the switching fetch. */
  openaiOptions(): { baseURL: string; apiKey: string; fetch: Fork["fetch"] } {
    return { baseURL: this.replay.openaiBaseUrl, apiKey: this.#token(), fetch: this.fetch };
  }

  /** Options for `new Anthropic(...)`, by the same rules. */
  anthropicOptions(): { baseURL: string; apiKey: string; fetch: Fork["fetch"] } {
    return { baseURL: this.replay.anthropicBaseUrl, apiKey: this.#token(), fetch: this.fetch };
  }

  /**
   * The fork's identity as environment variables, for an agent that tags its
   * own spans: `TRAZA_FORK_ID`, `TRAZA_FORK_PARENT`, `TRAZA_FORK_STEP`,
   * `TRAZA_FORK_RUN` (its session id), `TRAZA_REPLAY_ID`, and the replay's
   * provider base URLs. A subprocess cannot use the switching `fetch`: its
   * calls past the cut answer `409 replay_live`, for it to handle.
   */
  env(): Record<string, string> {
    return {
      ...this.replay.env(),
      TRAZA_FORK_ID: this.id,
      TRAZA_FORK_PARENT: this.parent,
      TRAZA_FORK_STEP: String(this.step),
      TRAZA_FORK_RUN: this.runId,
      TRAZA_REPLAY_ID: this.replay.id,
    };
  }

  /**
   * `parent` with this fork in it: spans started under the result (and
   * under every context derived from it) are the fork's. See {@link run}.
   */
  context<C extends OtelContext>(parent: C): C {
    return parent.setValue(FORK_CONTEXT_KEY, this) as C;
  }

  /**
   * Runs `fn` in the active OpenTelemetry context with this fork in it, so
   * {@link spanProcessor} tags the spans it starts — and only those. Pass
   * `context` from `@opentelemetry/api`.
   */
  run<T, C extends OtelContext>(api: OtelContextApi<C>, fn: () => T): T {
    return api.with(this.context(api.active()), fn);
  }

  /**
   * An OpenTelemetry span processor that stamps {@link spanAttributes} on
   * the spans started in this fork's context ({@link run}, {@link context})
   * while it is open. Register it with your tracer provider; it stops
   * tagging at {@link close}. A span started anywhere else — another
   * request the same server is handling — keeps its own run, unless
   * `allSpans: true` asks for every span.
   */
  spanProcessor(options: SpanProcessorOptions = {}): ForkSpanProcessor {
    const allSpans = options.allSpans ?? false;
    return {
      onStart: (span, parentContext) => {
        if (this.#closed) return;
        if (!allSpans && carriedFork(parentContext) !== this) return;
        for (const [key, value] of Object.entries(this.spanAttributes())) span.setAttribute(key, value);
      },
      onEnd: () => {},
      shutdown: async () => {},
      forceFlush: async () => {},
    };
  }

  /**
   * The next recorded tool step (of `name`), with an edited result where the
   * fork has one — or `null` when the harness should run the real tool: past
   * the cut, or with no recorded step of that tool left.
   *
   * `null` says nothing about model calls. The server's tool cursor is
   * independent of its model cursor, so a tool the recording does not hold
   * answers this way while recorded model calls are still to come, and the
   * switch keeps serving those from the recording until the model endpoint
   * itself answers `replay_live`.
   */
  async toolResult(name?: string, options: CallOptions = {}): Promise<ToolStep | null> {
    try {
      return await this.replay.nextTool(name, options);
    } catch (error) {
      if (error instanceof TrazaError && error.status === 409 && error.type === "replay_live") return null;
      throw error;
    }
  }

  /** Runs `tool` — unless the fork still has a recorded (or edited) result for it. */
  async callTool<T>(name: string, tool: () => T | Promise<T>): Promise<unknown> {
    const step = await this.toolResult(name);
    return step !== null ? step.result : tool();
  }

  /** The replay's status: what was served, the edits, the live point. */
  status(options: CallOptions = {}): Promise<ReplayStatus> {
    return this.replay.status(options);
  }

  /** The parent run against the forked run, once its spans are recorded under {@link runId}. */
  diff(options: CallOptions = {}): Promise<Diff> {
    return this.#client.diff(this.parent, this.runId, options);
  }

  /** Stops the span processor tagging. The replay itself expires on the server. */
  close(): void {
    this.#closed = true;
  }

  #token(): string {
    return this.#client.token || "traza-open";
  }

  /** Notes the model endpoint's `replay_live`: from here every model call goes to the real provider. */
  #modelsWentLive(): void {
    if (this.liveAt === null) this.liveAt = new Date();
  }

  async #switch(input: FetchInput, init: RequestInit): Promise<Response> {
    const request = await normalise(input, init);
    const routes: [string, { baseURL: string; apiKey: string | undefined }, "openai" | "anthropic"][] = [
      [this.replay.openaiBaseUrl, this.#openai, "openai"],
      [this.replay.anthropicBaseUrl, this.#anthropic, "anthropic"],
    ];
    const route = routes.find(([base]) => request.url === base || request.url.startsWith(base + "/"));
    if (route === undefined) return this.#transport(request.url, { ...init, body: request.body });
    const [replayBase, live, provider] = route;
    if (!this.isLive) {
      const headers = new Headers(request.headers);
      headers.delete("x-api-key");
      headers.set("authorization", `Bearer ${this.#token()}`);
      const answer = await this.#transport(request.url, { ...request.init, headers, body: request.body });
      if (!(this.live && answer.status === 409 && isLiveSignal(await answer.clone().text()))) return answer;
      this.#modelsWentLive();
    }
    const headers = new Headers(request.headers);
    headers.delete("authorization");
    headers.delete("x-api-key");
    if (live.apiKey) {
      if (provider === "openai") headers.set("authorization", `Bearer ${live.apiKey}`);
      else headers.set("x-api-key", live.apiKey);
    }
    return this.#transport(live.baseURL + request.url.slice(replayBase.length), {
      ...request.init,
      headers,
      body: request.body,
    });
  }
}

/** The fork an OpenTelemetry context carries, if it is one. */
function carriedFork(context: unknown): unknown {
  const getValue = (context as { getValue?: unknown } | null | undefined)?.getValue;
  return typeof getValue === "function" ? getValue.call(context, FORK_CONTEXT_KEY) : undefined;
}

function isLiveSignal(text: string): boolean {
  try {
    const body = JSON.parse(text) as { error?: { type?: unknown } };
    return typeof body.error === "object" && body.error !== null && body.error.type === "replay_live";
  } catch {
    return false;
  }
}

/** A request the switch may send twice: its body read once into something re-sendable. */
async function normalise(
  input: FetchInput,
  init: RequestInit,
): Promise<{ url: string; headers: Headers; body: BodyInit | undefined; init: RequestInit }> {
  let url: string;
  let headers: Headers;
  let body: BodyInit | null | undefined = init.body;
  let method = init.method;
  let signal = init.signal;
  if (typeof Request !== "undefined" && input instanceof Request) {
    url = input.url;
    headers = new Headers(init.headers ?? input.headers);
    method = method ?? input.method;
    signal = signal ?? input.signal;
    if (body === undefined && input.body !== null) body = await input.arrayBuffer();
  } else {
    url = String(input);
    headers = new Headers(init.headers);
  }
  if (typeof ReadableStream !== "undefined" && body instanceof ReadableStream) {
    body = await new Response(body).arrayBuffer();
  }
  const rest: RequestInit = { ...init, method, signal };
  delete rest.body;
  delete rest.headers;
  return { url, headers, body: body ?? undefined, init: rest };
}

function trimSlash(url: string): string {
  return url.replace(/\/+$/, "");
}

function processEnv(): Record<string, string | undefined> {
  const env = (globalThis as { process?: { env?: Record<string, string | undefined> } }).process?.env;
  return env ?? {};
}
