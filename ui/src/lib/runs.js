// Pure helpers for the run screens — Guard, Run, Diff. Kept out of the
// components so the arithmetic the screens state (how close a run is to a
// limit, what a fingerprint names, what a replay snippet says) is testable
// without rendering anything.

import { fmtDurationNs, fmtNum } from './format.js';

/** Every fuse limit, the usage figure it is compared with, and how to read
    both. The order is the order the guide's limit table uses. `used` reads
    the verdict document's `usage`; `>=` is the comparison (a run AT a limit
    has reached it), exactly as docs/guide/guard.md states.

    `min_success_odds` is the one limit that is not usage: a predictive fuse
    on how runs like this one have ended, a fraction in (0, 1] — so it draws
    no bar (`bar: false`) and takes a decimal (`decimal`, `max`). It is here
    because a grant of lower odds is how a foresight stop is lifted. Its two
    companions — the support it needs, the steps a run must have — are whole
    numbers with no bar either, and a policy may set them only beside it
    (`requires`), as the server insists; a grant may raise one alone. */
export const LIMITS = [
  { key: 'max_cost_usd', label: 'cost', used: (u) => u.cost_usd, fmt: (v) => '$' + fmtMoney(v) },
  { key: 'max_tokens', label: 'tokens', used: (u) => u.tokens, fmt: fmtNum },
  { key: 'max_steps', label: 'steps', used: (u) => u.steps, fmt: fmtNum },
  { key: 'max_llm_calls', label: 'LLM calls', used: (u) => u.llm_calls, fmt: fmtNum },
  { key: 'max_tool_calls', label: 'tool calls', used: (u) => u.tool_calls, fmt: fmtNum },
  {
    key: 'max_duration_seconds', label: 'duration', used: (u) => (u.duration_ns || 0) / 1e9,
    fmt: (v) => fmtDurationNs(v * 1e9),
  },
  { key: 'max_repeats', label: 'repeats', used: (u) => (u.top_repeat ? u.top_repeat.count : 0), fmt: fmtNum },
  { key: 'max_error_streak', label: 'error streak', used: (u) => u.error_streak, fmt: fmtNum },
  {
    key: 'max_handoff_cycles', label: 'handoff cycles',
    used: (u) => (u.top_handoff_cycle ? u.top_handoff_cycle.count : 0), fmt: fmtNum,
  },
  {
    key: 'min_success_odds', label: 'success odds', used: () => 0, fmt: (v) => String(v),
    bar: false, decimal: true, max: 1,
  },
  {
    key: 'foresight_min_support', label: 'foresight support', used: () => 0, fmt: fmtNum,
    bar: false, requires: 'min_success_odds',
  },
  {
    key: 'foresight_after_steps', label: 'foresight after steps', used: () => 0, fmt: fmtNum,
    bar: false, requires: 'min_success_odds',
  },
];

/** Money at the precision the figure deserves: cents for dollars, more
    digits for the fractions of a cent an individual model call costs. */
export function fmtMoney(value) {
  if (typeof value !== 'number' || !Number.isFinite(value)) return '—';
  if (value === 0) return '0.00';
  if (Math.abs(value) >= 1) return value.toFixed(2);
  if (Math.abs(value) >= 0.01) return value.toFixed(3);
  return value.toFixed(5);
}

/** The bars a verdict document draws: one per limit in force, with the
    fraction of it used. A limit absent from `limits` is unbounded and has no
    bar — drawing one against infinity would say nothing. */
export function usageBars(doc) {
  const limits = (doc && doc.limits) || {};
  const usage = (doc && doc.usage) || {};
  return LIMITS.filter((limit) => limit.bar !== false && typeof limits[limit.key] === 'number').map((limit) => {
    const max = limits[limit.key];
    const used = Number(limit.used(usage)) || 0;
    const fraction = max > 0 ? used / max : 0;
    return {
      key: limit.key,
      label: limit.label,
      used,
      max,
      fraction,
      // The guard's own thresholds: warn at 80%, the policy action at 100%.
      level: fraction >= 1 ? 'reached' : fraction >= 0.8 ? 'near' : 'ok',
      text: `${limit.fmt(used)} / ${limit.fmt(max)}`,
    };
  });
}

/** `kind:name#hash` split into its parts. The hash is exactly `#` plus 16
    hex characters (src/semconv.rs), so it is cut by shape rather than by
    searching for `#`, which a tool or span name may itself contain. */
export function splitFingerprint(fingerprint) {
  if (typeof fingerprint !== 'string' || !fingerprint) return { kind: '', name: '', hash: '' };
  const hashed = /#([0-9a-f]{16})$/.exec(fingerprint);
  const signature = hashed ? fingerprint.slice(0, -17) : fingerprint;
  const colon = signature.indexOf(':');
  return {
    kind: colon >= 0 ? signature.slice(0, colon) : '',
    name: colon >= 0 ? signature.slice(colon + 1) : signature,
    hash: hashed ? hashed[1] : '',
  };
}

/** The run id a session or trace opens as: the session id, else
    `trace:<trace_id>` — the same rule the server's run identity uses. */
export function runIdFor({ sessionId, traceId }) {
  if (sessionId) return sessionId;
  return traceId ? 'trace:' + traceId : '';
}

/** Parameters that carry a run's tenant, for a route or a request: `{tenant}`
    when the tenant is known — `''` for the default tenant, named explicitly —
    and undefined when it is not.

    The empty value matters. On the fuse routes naming no tenant happens to
    select the default one, but on tapes, diffs and replays naming none is the
    operator view across every tenant, and `tenant=` is what names the
    default tenant. A default-tenant run opened without it reads every
    tenant's run of that id, interleaved, while its verdict reads the default
    tenant alone. A bound credential is held to its own tenant either way. */
export function tenantParams(tenant) {
  return typeof tenant === 'string' ? { tenant } : undefined;
}

/** The tenant a route names: the `tenant` parameter's value (`''` naming the
    default tenant), or undefined when the route names none and the run's
    tenant is unknown. */
export function routeTenant(params) {
  return params && params.has('tenant') ? params.get('tenant') : undefined;
}

/** Whether a run read without a tenant is one tenant's: the server holds a
    bound credential to its own, so only an unbound one reads across every
    tenant. An identity not yet known counts as not one tenant's. */
export function singleTenantScope(tenant, identity) {
  return tenant !== undefined || Boolean(identity && identity.tenant);
}

/** A JSON value as a Python literal: `true`/`false`/`null` become
    `True`/`False`/`None`; strings, numbers, lists and objects keep JSON's
    spelling, which Python also reads. */
export function pyLiteral(value) {
  if (value === null || value === undefined) return 'None';
  if (value === true) return 'True';
  if (value === false) return 'False';
  if (typeof value === 'number') return Number.isFinite(value) ? String(value) : 'None';
  if (typeof value === 'string') return JSON.stringify(value);
  if (Array.isArray(value)) return '[' + value.map(pyLiteral).join(', ') + ']';
  if (typeof value === 'object') {
    return '{' + Object.entries(value).map(([k, v]) => JSON.stringify(k) + ': ' + pyLiteral(v)).join(', ') + '}';
  }
  return 'None';
}

/** The environment lines that point both SDKs at a replay.

    `base` is where the API is reachable from the machine running the agent
    (the page's origin plus any path prefix); the replay's own `base_urls`
    are paths under it. The key is the Traza token — both SDKs send it, as
    `Authorization: Bearer` and as `x-api-key` respectively, and the replay
    routes accept either. The token itself is never printed: a page that put
    a live credential into copyable text would put it into a screenshot. */
export function replayEnv(replay, base, auth) {
  const key = auth === 'open' ? 'unused  # an open server accepts any non-empty key' : '$TRAZA_TOKEN';
  return [
    `export OPENAI_BASE_URL=${base}${replay.base_urls.openai}`,
    `export OPENAI_API_KEY=${key}`,
    `export ANTHROPIC_BASE_URL=${base}${replay.base_urls.anthropic}`,
    `export ANTHROPIC_API_KEY=${key}`,
  ].join('\n');
}

/** Six lines of Python that make the run's first model call against the
    replay. When the tape recorded step 0's request as a message list, the
    example sends exactly those messages, so a strict replay matches it; else
    it says where the agent's own request goes. */
export function replayPython(tape) {
  const steps = (tape && tape.steps) || [];
  const first = steps.find((step) => step.kind === 'llm');
  const recorded = first && Array.isArray(first.request) && first.request.every((m) => m && typeof m === 'object' && 'role' in m);
  const messages = recorded ? pyLiteral(first.request) : '[{"role": "user", "content": "…"}]';
  const model = (first && first.model) || 'gpt-4o';
  return [
    'from openai import OpenAI',
    'client = OpenAI()  # reads OPENAI_BASE_URL and OPENAI_API_KEY',
    `messages = ${messages}  # ${recorded ? `step ${first.index} as recorded` : "your agent's first request"}`,
    `reply = client.chat.completions.create(model=${JSON.stringify(model)}, messages=messages)`,
    'print(reply.choices[0].message)  # the recorded answer; no model was called',
    'print(reply.usage)  # the recorded token usage',
  ].join('\n');
}

/** Totals over a tape's steps: what the Run screen's tiles say. Tokens and
    cost count only what the steps reported — a missing figure is not zero,
    so `priced` says how many steps carried a cost at all. */
export function tapeTotals(tape) {
  const steps = (tape && tape.steps) || [];
  if (!steps.length) return { steps: 0, tokens: 0, cost: 0, priced: 0, errors: 0, startNs: 0, endNs: 0, durationNs: 0 };
  let tokens = 0;
  let cost = 0;
  let priced = 0;
  let errors = 0;
  let startNs = Infinity;
  let endNs = -Infinity;
  for (const step of steps) {
    tokens += (step.tokens?.input || 0) + (step.tokens?.output || 0);
    if (typeof step.cost_usd === 'number') { cost += step.cost_usd; priced += 1; }
    if (step.status === 'error') errors += 1;
    startNs = Math.min(startNs, step.start_ns);
    endNs = Math.max(endNs, step.start_ns + (step.duration_ns || 0));
  }
  return { steps: steps.length, tokens, cost, priced, errors, startNs, endNs, durationNs: endNs - startNs };
}
