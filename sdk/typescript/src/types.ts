/**
 * Response shapes, field for field as the server sends them.
 *
 * Field names are the wire's own (snake_case), so these types read the same
 * as the HTTP reference and no field is renamed on the way in. Every
 * `*_ns` field is a `bigint` — Unix nanoseconds do not fit a double exactly
 * (see `json.ts`). Fields the server may omit are optional.
 *
 * @module
 */

/** Unix nanoseconds, exact. */
export type Nanos = bigint;

// ---------------------------------------------------------------- identity

/** `GET /v1/whoami`. */
export interface Identity {
  /** `token`, or `open` when the server has no credential at all. */
  auth: "token" | "open";
  /** `admin` in open mode, which permits everything. */
  scope: "ro" | "rw" | "admin";
  /** The binding; `null` for an unbound credential. */
  tenant: string | null;
  /** `true` for an unbound credential: it may use the operator surfaces. */
  operator: boolean;
  /** The credential that authenticated; `null` in open mode. */
  credential: { id: string; label: string } | null;
}

// ---------------------------------------------------------------- usage

export interface TenantLimits {
  monthly_spans: number | null;
  monthly_bytes: number | null;
  ingest_paused: boolean;
  ttl_seconds: number | null;
}

export interface TenantUsage {
  /** `""` is the default tenant. */
  tenant: string;
  lifetime: { spans: number; bytes: number; batches: number; rejected: number };
  /** `window` is the UTC calendar month, `YYYY-MM`. */
  month: { window: string; spans: number; bytes: number };
  limits: TenantLimits;
  state: "ok" | "warning" | "capped" | "paused";
}

/** `GET /v1/usage`. */
export interface UsageReport {
  as_of_ns: Nanos;
  tenants: TenantUsage[];
}

/** The `quota` object of a `429` quota or pause refusal. */
export interface QuotaRefusal {
  tenant: string;
  dimension: "monthly_spans" | "monthly_bytes" | "paused";
  limit: number | null;
  used: number;
  window: string;
  resets_at: string | null;
}

// ---------------------------------------------------------------- fuses

export interface SpanRef {
  trace_id: string;
  span_id: string;
}

export interface RunUsage {
  steps: number;
  llm_calls: number;
  tool_calls: number;
  tokens: number;
  cost_usd: number;
  errors: number;
  error_streak: number;
  duration_ns: Nanos;
  /** The step fingerprint seen most often, or `null`. */
  top_repeat: { fingerprint: string; count: number; example: SpanRef } | null;
  /**
   * The most frequent handoff cycle since the last reset — control
   * returning to `agents[0]` straight from `agents[1]` — absent when there
   * is none.
   */
  top_handoff_cycle?: { agents: [string, string]; count: number };
}

/** A fuse policy's limits, or a reset's grant. An absent limit is unbounded. */
export interface Limits {
  max_cost_usd?: number;
  max_tokens?: number;
  max_steps?: number;
  max_llm_calls?: number;
  max_tool_calls?: number;
  max_duration_seconds?: number;
  max_repeats?: number;
  max_error_streak?: number;
  max_handoff_cycles?: number;
}

/**
 * A nudge a pre-call check delivered: `advice` for the agent to hear, or a
 * `block` that vetoed the step it asked about.
 */
export interface DeliveredNudge {
  id: string;
  kind: "advice" | "block";
  message: string;
}

/** The verdict document every run-fuse route answers. */
export interface Verdict {
  run: string;
  tenant: string;
  verdict: "continue" | "warn" | "stop";
  reasons: string[];
  /** The policy the run is held to; `global` for the server default. */
  policy: string;
  usage: RunUsage;
  limits: Limits;
  grant: Limits | null;
  tripped_at_ns: Nanos | null;
  /** `null` for a hypothetical stop from `check`. */
  tripped_by: "policy" | "manual" | null;
  evidence: SpanRef[];
  source: "live" | "rebuilt";
  /**
   * The nudges this answer delivered: present on {@link Client.check}
   * answers only. Each nudge is delivered once — the check that carries it
   * consumes it, read or not — so read them here, or pass `onNudges` to
   * `guard`.
   */
  nudges?: DeliveredNudge[];
}

export interface Policy {
  id: string;
  tenant: string;
  name: string;
  /** `{}` for the tenant's `default` policy. */
  match: { service?: string };
  limits: Limits;
  action: "stop" | "warn";
  created_at_ns: Nanos;
}

/** `GET /v1/fuses/policies`. */
export interface Policies {
  policies: Policy[];
  /** The server-wide default from the `--fuse-*` flags. */
  global: { name: "global"; action: "stop" | "warn"; limits: Limits };
}

/** One verdict transition from `GET /v1/fuses/events`. */
export interface FuseEvent {
  cursor: string;
  at_ns: Nanos;
  tenant: string;
  run: string;
  kind: "warn" | "stop" | "manual_stop" | "reset";
  reasons: string[];
  policy: string;
}

// ---------------------------------------------------------------- tapes

/** Token counts a step reported; `null` when not reported, never 0. */
export interface StepTokens {
  input: number | null;
  output: number | null;
  cache_read: number | null;
  cache_write: number | null;
}

export interface Step {
  index: number;
  kind: "llm" | "tool" | "agent";
  name: string;
  service: string;
  status: string;
  trace_id: string;
  span_id: string;
  parent_span_id?: string | null;
  start_ns: Nanos;
  duration_ns: Nanos;
  model?: string;
  tool?: string;
  agent?: string;
  /** The input as stored; a `{"$payload": …}` reference unless resolved. */
  request?: unknown;
  response?: unknown;
  request_digest?: string;
  response_digest?: string;
  /** `kind:name#` plus 16 hex characters of the request digest. */
  fingerprint: string;
  tokens: StepTokens;
  cost_usd?: number;
}

/** `GET /v1/tapes/{run}`. */
export interface Tape {
  run: string;
  /** The tenant scope read under; `null` is the operator view. */
  tenant: string | null;
  steps: Step[];
  truncated: boolean;
  spans_examined: number;
  /** Only with `resolve`: some payload reference stayed a reference. */
  resolved_partial?: boolean;
}

// ---------------------------------------------------------------- replays

/** `POST /v1/replays`. */
export interface ReplayDocument {
  id: string;
  run: string;
  mode: "strict" | "loose";
  steps: number;
  llm_steps: number;
  expires_at_ns: Nanos;
  unreadable_tool_steps: number[];
  /** Paths on the server: `{openai: "/v1/replays/…/openai/v1", anthropic: …}`. */
  base_urls: { openai: string; anthropic: string };
}

export interface Divergence {
  step: number;
  expected_digest: string;
  got_digest: string;
}

/** `GET /v1/replays/{id}`. */
export interface ReplayStatus {
  id: string;
  run: string;
  mode: "strict" | "loose";
  served: number;
  remaining: number;
  diverged: Divergence | null;
  calls: { step: number; matched: boolean; at_ns: Nanos }[];
  unreadable_tool_steps: number[];
  expires_at_ns: Nanos;
}

/** A recorded tool step, from `POST /v1/replays/{id}/tools/next`. */
export interface ToolStep {
  step: number;
  tool: string;
  arguments: unknown;
  result: unknown;
}

/** The environment that points both providers' SDKs at a replay. */
export interface ReplayEnv {
  OPENAI_BASE_URL: string;
  OPENAI_API_KEY: string;
  ANTHROPIC_BASE_URL: string;
  ANTHROPIC_API_KEY: string;
  [name: string]: string;
}

// ---------------------------------------------------------------- diff

export interface RunSummary {
  run: string;
  steps: number;
  cost_usd: number;
  tokens: number;
  duration_ns: Nanos;
  errors: number;
  outcome: "success" | "failure" | "abandoned" | "unknown";
  truncated: boolean;
}

export interface AlignedStep {
  op: "same" | "changed" | "insert" | "delete";
  a: number | null;
  b: number | null;
  fingerprint_a: string | null;
  fingerprint_b: string | null;
  /** Absent when `op` is `same`. */
  why?: "input" | "decision" | "config" | "structure";
}

/** `GET /v1/diff`. */
export interface Diff {
  a: RunSummary;
  b: RunSummary;
  aligned: AlignedStep[];
  first_divergence: {
    a: number;
    b: number;
    kind: "input" | "decision" | "config" | "structure";
    explanation: string;
  } | null;
  /** `b` minus `a`. */
  delta: { cost_usd: number; tokens: number; duration_ns: Nanos; errors: number; steps: number };
  truncated: boolean;
}

// ---------------------------------------------------------------- recall

export interface RecalledRun {
  run: string;
  tenant: string;
  outcome: "success" | "failure" | "abandoned" | "unknown";
  outcome_source: "declared" | "derived";
  goal?: string;
  /** How many of the run's spans matched the query. */
  score: number;
  last_end_ns: Nanos;
  steps: number;
  cost_usd: number;
  /** Step signatures in order, consecutive repeats folded (`tool:x x3`). */
  skeleton: string[];
  skeleton_truncated: boolean;
  truncated: boolean;
  cause?: { trace_id: string; span_id: string; name: string; rule: string };
}

/** `GET /v1/recall`. */
export interface Recall {
  runs: RecalledRun[];
  truncated: boolean;
}

// ---------------------------------------------------------------- ledger

/** `GET /v1/ledger/head`. */
export interface LedgerHead {
  seq: number;
  head: string;
  store_id: string;
  at_ns: Nanos;
}

/** `GET /v1/evidence`. See the ledger guide for `ledger` and `erasures`. */
export interface EvidenceDocument {
  format: string;
  digest: string;
  engine_version: string;
  generated_at_ns: Nanos;
  store_id: string;
  tenant: string | null;
  window: { since_ns: Nanos; until_ns: Nanos };
  ledger: {
    head: LedgerHead;
    entries: Record<string, unknown>[];
    entries_truncated: boolean;
    entries_unreadable: number;
    verification: { intact: boolean; problems: string[]; [field: string]: unknown };
  };
  retention: { ttl_seconds: number | null; source: string };
  usage: TenantUsage | null;
  activity: {
    spans: number;
    traces: number;
    sessions: number;
    errors: number;
    by_day: { day: string; spans: number; errors: number; cost_usd: number }[];
  };
  erasures: Record<string, unknown>[];
}

/** An evidence pack, with the check a notary runs before signing it. */
export interface EvidencePack extends EvidenceDocument {
  /** Recomputes `digest` over the bytes the server sent. */
  verifyDigest(): Promise<boolean>;
}
