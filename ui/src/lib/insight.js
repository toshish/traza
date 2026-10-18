// Pure helpers for the insight screens — Foresight, X-Ray, Changes,
// Behaviors and Blame. Kept out of the components so the arithmetic each
// screen states (where a branch sits, which colour a success rate earns,
// which chunks one step put in a context) is testable without rendering.

import { fmtDurationNs, fmtNum } from './format.js';
import { fmtMoney } from './runs.js';

// ------------------------------------------------------------------ windows

/** The windows the insight screens offer. `30d` is every route's own
    default, so it sends no `since` at all — which is also the build the
    server caches per tenant (a Fate Map asked for with an explicit window
    is built again for every distinct window). */
export const WINDOWS = [
  { id: '24h', label: '24h', days: 1 },
  { id: '7d', label: '7d', days: 7 },
  { id: '30d', label: '30d', days: 30 },
  { id: '90d', label: '90d', days: 90 },
  { id: '1y', label: '1y', days: 365 },
];

export const DEFAULT_WINDOW = '30d';

/** `since` for a window id, as integer Unix nanoseconds in a string, or
    undefined for the server's default window. Floored to the hour so a
    screen re-read within the hour asks for the same window (and a cached
    build can answer it); computed in BigInt because a nanosecond timestamp
    is past 2^53 and a float would round it. */
export function windowSince(id, nowMs = Date.now()) {
  const window = WINDOWS.find((w) => w.id === id);
  if (!window || window.id === DEFAULT_WINDOW) return undefined;
  const hour = 3600 * 1000;
  const ms = Math.floor((nowMs - window.days * 86400 * 1000) / hour) * hour;
  return (BigInt(ms) * 1000000n).toString();
}

/** A window id from a route parameter, falling back to the default. */
export function windowFrom(params) {
  const id = params && params.get ? params.get('window') : null;
  return WINDOWS.some((w) => w.id === id) ? id : DEFAULT_WINDOW;
}

// ------------------------------------------------------------------- rates

/** A rate as a percent with one decimal: "54.4%"; "—" when there is none. */
export function fmtPct(rate, digits = 1) {
  if (typeof rate !== 'number' || !Number.isFinite(rate)) return '—';
  return (rate * 100).toFixed(digits) + '%';
}

/** A Wilson interval as "43.5–61.6%". */
export function fmtCi(ci, digits = 1) {
  if (!Array.isArray(ci) || ci.length !== 2) return '—';
  return (ci[0] * 100).toFixed(digits) + '–' + (ci[1] * 100).toFixed(digits) + '%';
}

/** A p-value at the precision a reader can use: "<0.001", "0.012", "0.27". */
export function fmtP(p) {
  if (typeof p !== 'number' || !Number.isFinite(p)) return '—';
  if (p < 0.001) return '<0.001';
  if (p < 0.01) return p.toFixed(3);
  return p.toFixed(2);
}

/** Whether a 95% interval contains a rate: the evidence cannot tell the two
    apart, so a colour drawn from the point estimate is drawn faint. */
export function ciContains(ci, rate) {
  return Array.isArray(ci) && typeof rate === 'number' && ci[0] <= rate && rate <= ci[1];
}

/** The 95% Wilson score interval of `successes` in `n`, as the server
    computes every `ci95`; null when there are no trials. */
export function wilson(successes, n) {
  if (!(n > 0)) return null;
  const z = 1.959963984540054;
  const p = successes / n;
  const z2 = z * z;
  const denom = 1 + z2 / n;
  const center = (p + z2 / (2 * n)) / denom;
  const half = (z * Math.sqrt((p * (1 - p)) / n + z2 / (4 * n * n))) / denom;
  return [Math.max(0, center - half), Math.min(1, center + half)];
}

// ------------------------------------------------------------- the Fate Map

/** How far below the agent's base rate a branch's success rate may fall
    before it takes the next step of the measured ramp. A branch at or above
    the base rate is ink: the eye should go to where runs die, and the ramp
    is one hue — more of it means worse, never a second meaning. */
export const RAMP_STEPS = [
  { below: 0.1, color: 'var(--measure-2)', label: '0–10' },
  { below: 0.2, color: 'var(--measure-3)', label: '10–20' },
  { below: 0.35, color: 'var(--measure-4)', label: '20–35' },
  { below: Infinity, color: 'var(--measure-5)', label: '35+' },
];

export const AT_OR_ABOVE = 'var(--series-3)';

/** The colour a branch earns: ink at or above the base rate, the measured
    ramp below it by how many points. */
export function rateColor(rate, base) {
  if (typeof rate !== 'number' || typeof base !== 'number') return 'var(--series-4)';
  const gap = base - rate;
  if (gap <= 0) return AT_OR_ABOVE;
  return RAMP_STEPS.find((step) => gap < step.below).color;
}

/** A path as a key: the tokens joined by a separator no token holds. */
export function pathKey(path) {
  return (path || []).join('\u0001');
}

/** `kind:name` (with a repeat bucket) split for display. The kind is the
    text before the first colon; a model name may itself hold colons. */
export function splitToken(token) {
  if (typeof token !== 'string') return { kind: '', name: '' };
  if (token === '^' || token === '…') return { kind: '', name: token };
  const colon = token.indexOf(':');
  return colon < 0 ? { kind: '', name: token } : { kind: token.slice(0, colon), name: token.slice(colon + 1) };
}

/** A token cut to `max` characters for a column label, keeping the repeat
    bucket (`×3-5`) visible: it is the part that tells a loop from a call. */
export function shortName(name, max = 22) {
  if (typeof name !== 'string' || name.length <= max) return name;
  const bucket = /×(2|3-5|6\+)$/.exec(name);
  const tail = bucket ? bucket[0] : '';
  return name.slice(0, Math.max(1, max - tail.length - 1)) + '…' + tail;
}

/** A long path cut to at most `max` items for a panel: the first token,
    a window around `focus` (where the map stopped matching, or the token
    in focus), and the last token, with the runs between folded into a gap
    that says how many tokens it holds. Each item is `{token, index}` or
    `{gap}`. */
export function compactPath(path, focus, max = 9) {
  const list = path || [];
  if (list.length <= max) return list.map((token, index) => ({ token, index }));
  const keep = new Set([0, list.length - 1]);
  const around = Math.max(1, max - 4);
  const center = Math.min(list.length - 2, Math.max(1, typeof focus === 'number' ? focus : 1));
  let lo = Math.max(1, center - Math.floor(around / 2));
  const hi = Math.min(list.length - 2, lo + around - 1);
  lo = Math.max(1, hi - around + 1);
  for (let i = lo; i <= hi; i += 1) keep.add(i);
  const out = [];
  let last = -1;
  for (const index of [...keep].sort((a, b) => a - b)) {
    if (index - last > 1) out.push({ gap: index - last - 1 });
    out.push({ token: list[index], index });
    last = index;
  }
  return out;
}

/** Stroke width for a branch: linear in visits, so twice the runs is twice
    the width, floored so a three-run branch stays visible. */
export function branchWidth(visits, rootVisits, max = 18) {
  if (!rootVisits) return 1.5;
  return Math.max(1.5, (visits / rootVisits) * max);
}

/** Lays a Fate Map out left to right.

    Each leaf takes one row — tall enough for its branch's width and its two
    label lines — and a parent sits midway between its first and last child,
    so siblings read top to bottom in the server's order (most visited
    first). Depth is the column. Returns positioned nodes (each with its
    path and key), the edges between them, and the canvas size.

    `collapsed` holds keys whose children are not drawn. */
export function layoutFate(root, { colWidth = 184, padX = 28, padY = 26, collapsed = new Set(), maxStroke = 18 } = {}) {
  if (!root) return { nodes: [], edges: [], width: 0, height: 0, byKey: new Map() };
  const rootVisits = root.visits || 0;
  const nodes = [];
  const edges = [];
  const byKey = new Map();
  let cursor = padY;
  let maxDepth = 0;

  const place = (node, path, depth, parent) => {
    const key = pathKey(path);
    const width = branchWidth(node.visits, rootVisits, maxStroke);
    const children = collapsed.has(key) ? [] : (node.children || []);
    const entry = {
      node, key, path, depth, parentKey: parent ? parent.key : null,
      x: padX + depth * colWidth, y: 0, stroke: width,
      hiddenChildren: collapsed.has(key) ? (node.children || []).length : 0,
    };
    nodes.push(entry);
    byKey.set(key, entry);
    maxDepth = Math.max(maxDepth, depth);
    if (!children.length) {
      const row = Math.max(58, width + 44);
      entry.y = cursor + row / 2;
      cursor += row;
    } else {
      const placed = children.map((child) => place(child, [...path, child.token], depth + 1, entry));
      entry.y = (placed[0].y + placed[placed.length - 1].y) / 2;
      for (const child of placed) edges.push({ from: entry, to: child, width: child.stroke });
    }
    return entry;
  };

  place(root, [], 0, null);
  return {
    nodes, edges, byKey,
    width: padX * 2 + maxDepth * colWidth + 150,
    height: cursor + padY,
  };
}

/** The tree's node at a path, or null. */
export function nodeAt(root, path) {
  let node = root;
  for (const token of path || []) {
    node = (node && node.children || []).find((child) => child.token === token);
    if (!node) return null;
  }
  return node;
}

/** What a run's foresight lights up on its agent's map: the keys of every
    node its path matched, the node it is judged at (and, for a live run
    whose last step is open, the pooled siblings counted with it), where it
    left the known paths, and the option earlier runs did best with.

    The matched path is walked on the map as drawn. Foresight walks the
    whole tree, so a run can be judged deeper than the `depth` the map was
    asked for, or at a node `min_support` pruned. The walk then stops short
    (`complete` false): only the drawn prefix is lit, and nothing that
    belongs to the judged node — THIS RUN, its pooled siblings, the
    recommended option, a novel run's divergence — is placed on an ancestor
    that merely shares a token with it. `reachedKey` is the deepest node
    drawn; `unmatched` holds what the run did past it. */
export function runOnMap(root, foresight) {
  const empty = {
    keys: new Set(), pooled: new Set(), atKey: null, reachedKey: null, unmatched: [], recommendedKey: null,
    divergence: null, complete: false, walked: 0, matchedDepth: 0,
  };
  if (!root || !foresight || !Array.isArray(foresight.path)) return empty;
  const path = foresight.path;
  const depth = Math.min(foresight.matched_depth || 0, path.length);
  const keys = new Set([pathKey([])]);
  let node = root;
  let walked = 0;
  for (let i = 0; i < depth; i += 1) {
    const next = (node.children || []).find((child) => child.token === path[i]);
    if (!next) break;
    node = next;
    walked = i + 1;
    keys.add(pathKey(path.slice(0, walked)));
  }
  const atPath = path.slice(0, walked);
  const complete = walked === depth;
  const out = {
    ...empty, keys, reachedKey: pathKey(atPath), unmatched: path.slice(walked), complete, walked, matchedDepth: depth,
  };
  if (!complete) return out;
  const parentPath = path.slice(0, Math.max(0, walked - 1));
  const pooled = new Set();
  if (foresight.last_open && foresight.node && Array.isArray(foresight.node.pooled) && walked > 0) {
    for (const token of foresight.node.pooled) {
      if (nodeAt(root, [...parentPath, token])) pooled.add(pathKey([...parentPath, token]));
    }
  }
  let recommendedKey = null;
  if (foresight.recommended_next) {
    const candidate = [...atPath, foresight.recommended_next];
    if (nodeAt(root, candidate)) recommendedKey = pathKey(candidate);
  }
  return { ...out, pooled, atKey: pathKey(atPath), recommendedKey, divergence: foresight.divergence || null };
}

/** The route changes that would draw the node foresight judged a run at,
    when `runOnMap` stopped short: a deeper `depth` if the map was cut
    above it, and the lowest support if the walk stopped inside the drawn
    depth (a pruned node). `choices` are the depth chips; a depth past them
    is asked for exactly (the server takes up to 65). */
export function drawRunChanges(lit, map, choices) {
  if (!lit || lit.complete || !map) return null;
  const changes = {};
  if (lit.matchedDepth > (map.depth || 0)) {
    const need = lit.matchedDepth + 1;
    changes.depth = choices.find((d) => d >= need) || Math.min(65, need);
  }
  if (lit.walked < Math.min(map.depth || 0, lit.matchedDepth)) changes.min = 1;
  return changes;
}

/** A node's means, from the sums the map carries: cost and duration per
    run through it. */
export function nodeMeans(node) {
  const visits = node && node.visits;
  if (!visits) return { cost: null, durationNs: null };
  return {
    cost: typeof node.cost_usd_sum === 'number' ? node.cost_usd_sum / visits : null,
    durationNs: typeof node.duration_ns_sum === 'number' ? node.duration_ns_sum / visits : null,
  };
}

/** A map path as the TQL query that finds exactly the runs through its
    node, anchored at the run's first step.

    Each token is matched by its step signature (`kind:name`), compared
    exactly as the map groups steps — never by the atom's own name test,
    which ignores ASCII case and would take `Search` for `search` — and its
    repeat bucket becomes a count. The map puts a run that repeated a step
    more than its bucket allows in a sibling branch (three to five calls is
    `×3-5`, six is `×6+`), so the last token is held to its bucket: the run
    ends there, or its next step is something else. A token before it needs
    no such guard: the next token is a different signature, so the very next
    step must be it.

    `null` for a path TQL cannot say exactly: the fold token or a cut name
    (ending in `…`), a kind that is not a step, a name that itself carries
    the bucket mark `×`, or — given the map's `root` — a token two branches
    of the map share. The map names a step by its text, so a tool literally
    called `search×2`, called once, reads like `search` called twice; when
    both are drawn under one node there is no telling which branch a query
    would mean. */
export function pathToTql(path, root) {
  if (!Array.isArray(path) || !path.length) return null;
  const atoms = [];
  let node = root || null;
  let last = null;
  for (const token of path) {
    if (root) {
      const children = (node && node.children) || [];
      if (children.filter((child) => child.token === token).length > 1) return null;
      node = children.find((child) => child.token === token) || null;
    }
    const { kind, name } = splitToken(token);
    if (!['llm', 'tool', 'agent'].includes(kind)) return null;
    const bucket = /×(2|3-5|6\+)$/.exec(name);
    const bare = bucket ? name.slice(0, -bucket[0].length) : name;
    if (!bare || bare.endsWith('…') || bare.includes('×') || /[\u0000-\u001f]/.test(bare)) return null;
    const signature = JSON.stringify(`${kind}:${bare}`);
    const count = !bucket ? '' : bucket[1] === '2' ? '{2}' : bucket[1] === '3-5' ? '{3,5}' : '{6,}';
    atoms.push(`${kind}[signature=${signature}]${count}`);
    last = { signature, open: Boolean(bucket) && bucket[1] === '6+' };
  }
  const head = '^' + atoms.join(' -> ');
  // Six or more has no upper bound: nothing after it can move a run out.
  if (last.open) return head;
  return `${head}$ | ${head} -> step[signature!=${last.signature}]`;
}

/** The verdicts foresight gives a run, with the tag tone each is drawn in. */
export const FORESIGHT_VERDICT = {
  on_track: { tone: 'ok', label: 'on track' },
  at_risk: { tone: 'warn', label: 'at risk' },
  likely_failure: { tone: 'error', label: 'likely failure' },
  novel: { tone: 'warn', label: 'novel' },
  insufficient_history: { tone: 'neutral', label: 'insufficient history' },
};

/** Runs a calibration depth must have been scored on before its skill is
    a headline or a run's verdict is read against it. Each run is
    predicted from half the cohort, so a depth only a handful of runs
    reached can win on the split alone. */
export const MIN_SCORED_RUNS = 30;

function scored(calibration, minCovered) {
  return ((calibration && calibration.depths) || [])
    .filter((row) => typeof row.skill === 'number' && (row.covered || 0) >= minCovered);
}

/** The depth whose prediction removed the most of the base rate's error,
    among those scored on enough runs; null when none was. */
export function bestSkill(calibration, minCovered = MIN_SCORED_RUNS) {
  let best = null;
  for (const row of scored(calibration, minCovered)) {
    if (!best || row.skill > best.skill) best = row;
  }
  return best;
}

/** The scored depth nearest to `depth` — the evidence for a verdict given
    at that depth — the shallower on a tie; null for a run matched nowhere,
    or a map that scored no depth on enough runs. */
export function skillAt(calibration, depth, minCovered = MIN_SCORED_RUNS) {
  if (!(depth > 0)) return null;
  let near = null;
  for (const row of scored(calibration, minCovered)) {
    const gap = Math.abs(row.depth - depth);
    if (!near || gap < Math.abs(near.depth - depth) || (gap === Math.abs(near.depth - depth) && row.depth < near.depth)) near = row;
  }
  return near;
}

/** How to read a map's skill: honest words for how much its verdicts are
    worth. Below 5% the map is no better than assuming the average. */
export function skillReading(skill) {
  if (typeof skill !== 'number') return 'not scored';
  if (skill < 0.05) return 'no better than the base rate';
  if (skill < 0.15) return 'a little better than the base rate';
  if (skill < 0.3) return 'clearly better than the base rate';
  return 'much better than the base rate';
}

// ------------------------------------------------------------------ X-Ray

/** The colour class of a chunk, by what kind of content it is. Ink at four
    weights plus paper: instructions darkest, the model's own words
    lightest, tool definitions as a well. The accent is kept for the one
    step in focus. */
export const CHUNK_CLASSES = [
  { id: 'definitions', label: 'tool definitions', fill: 'var(--bg-raised)' },
  { id: 'system', label: 'system', fill: 'var(--series-1)' },
  { id: 'user', label: 'user', fill: 'var(--series-2)' },
  { id: 'tool_result', label: 'tool results', fill: 'var(--series-3)' },
  { id: 'model', label: 'model output and calls', fill: 'var(--series-4)' },
  { id: 'other', label: 'other', fill: 'var(--bg-raised)' },
];

export function chunkClass(chunk) {
  if (!chunk) return 'other';
  if (chunk.role === 'tools') return 'definitions';
  if (chunk.kind === 'system') return 'system';
  if (chunk.kind === 'user') return 'user';
  if (chunk.kind === 'tool_result') return 'tool_result';
  if (chunk.kind === 'assistant' || chunk.kind === 'tool_call') return 'model';
  return 'other';
}

export function chunkFill(chunk) {
  const id = chunkClass(chunk);
  return CHUNK_CLASSES.find((c) => c.id === id).fill;
}

/** The key of the step that introduced a chunk, or `self` when none earlier
    did (the call's own new content, or provenance unknown). */
export function provenanceKey(chunk) {
  const by = chunk && chunk.introduced_by;
  return by && typeof by.step_index === 'number' ? 's' + by.step_index : 'self';
}

/** A context's chunks grouped by the step that introduced them, most tokens
    first: "61% of this context is what step 7 returned". */
export function provenanceGroups(chunks) {
  const groups = new Map();
  let total = 0;
  for (const chunk of chunks || []) {
    const key = provenanceKey(chunk);
    const by = chunk.introduced_by;
    let group = groups.get(key);
    if (!group) {
      group = {
        key, step: by && typeof by.step_index === 'number' ? by.step_index : null,
        kind: by ? by.kind : null, name: by ? by.name : null, via: by ? by.via : null,
        span: by ? { trace_id: by.trace_id, span_id: by.span_id } : null,
        tokens: 0, chunks: 0, stale: 0, indices: [],
      };
      groups.set(key, group);
    }
    group.tokens += chunk.est_tokens || 0;
    group.chunks += 1;
    if (chunk.stale) group.stale += 1;
    group.indices.push(chunk.index);
    total += chunk.est_tokens || 0;
  }
  return [...groups.values()]
    .map((group) => ({ ...group, share: total ? group.tokens / total : 0 }))
    .sort((a, b) => b.tokens - a.tokens || (a.step ?? -1) - (b.step ?? -1));
}

/** Token totals per colour class, in the order the legend draws them. */
export function classTotals(chunks) {
  const totals = new Map(CHUNK_CLASSES.map((c) => [c.id, 0]));
  for (const chunk of chunks || []) totals.set(chunkClass(chunk), totals.get(chunkClass(chunk)) + (chunk.est_tokens || 0));
  return CHUNK_CLASSES.filter((c) => totals.get(c.id) > 0).map((c) => ({ ...c, tokens: totals.get(c.id) }));
}

/** The Cache Autopsy's causes, the tag tone each is drawn in, and whether
    it is a break. `stable` is the cache doing its job; `new_context` is no
    predecessor, or a rewrite; the rest lost tokens the cache could have
    served. */
export const CACHE_CAUSE = {
  stable: { tone: 'ok', label: 'stable' },
  volatile_content: { tone: 'error', label: 'volatile content' },
  reordered: { tone: 'error', label: 'reordered' },
  ttl_expired: { tone: 'warn', label: 'ttl expired' },
  new_context: { tone: 'neutral', label: 'new context' },
  unknown: { tone: 'neutral', label: 'unknown' },
};

/** One call's bar in the autopsy timeline: its context split into the
    prefix the cache could serve, the tokens it lost (re-processed although
    the predecessor had them), and what is new. Estimates throughout, in
    one unit: the server scales `est_tokens_lost` by the provider's count
    (`calibration`) while `est_tokens` stays the raw estimate, so the loss
    is taken back to the raw scale before it is set against the context.

    The leading chunks byte-identical to the predecessor's
    (`shared_est_tokens`) stay servable through a break — the loss is what
    follows the divergence — except past the TTL, where the server counts
    the prefix in the loss, and for `unknown`, where whether even a held
    prefix hits cannot be told. The part of an edited chunk before the
    change is neither shared nor lost, so it is drawn with the new. */
export function cacheSegments(step) {
  const total = Math.max(0, step && step.est_tokens || 0);
  const reported = Math.max(0, step && step.est_tokens_lost || 0);
  const raw = step && typeof step.calibration === 'number' && step.calibration > 0 ? reported / step.calibration : reported;
  const lost = Math.min(total, Math.round(raw));
  const servable = step && step.cause !== 'ttl_expired' && step.cause !== 'unknown';
  const served = servable ? Math.max(0, Math.min(total - lost, step.shared_est_tokens || 0)) : 0;
  const fresh = Math.max(0, total - lost - served);
  return { total, served, lost, fresh };
}

/** Whether a call plainly contradicts its verdict: a prefix that held, yet
    the provider reported no cache read. (The summary's `disagreements`
    also counts the other way round; this marks only the case one call's
    own fields settle.) */
export function stableWithoutRead(step) {
  return Boolean(step && step.cause === 'stable' && step.cache_read === 0);
}

/** Estimated money as the autopsy reports it: `null` means no rate. */
export function fmtLostUsd(value) {
  if (value == null) return '—';
  if (value === 0) return '0.00';
  return fmtMoney(value);
}

// ---------------------------------------------------------------- Changes

/** The deltas a change carries, how to read each, and which way is worse. */
export const DELTA_METRICS = [
  { key: 'error_rate', label: 'error rate', worse: 'up', fmt: (v) => fmtPct(v) },
  { key: 'success_rate', label: 'success rate', worse: 'down', fmt: (v) => fmtPct(v) },
  { key: 'cost_per_run', label: 'cost per run', worse: 'up', fmt: (v) => '$' + fmtMoney(v) },
  { key: 'steps_per_run', label: 'steps per run', worse: 'up', fmt: (v) => (typeof v === 'number' ? v.toFixed(1) : '—') },
  { key: 'duration_ns_per_run', label: 'duration per run', worse: 'up', fmt: (v) => (typeof v === 'number' ? fmtDurationNs(v) : '—') },
];

/** Whether a delta moved the bad way, at the 5% level before correction.
    The verdict is the server's (Holm-corrected); this only marks a row. */
export function deltaDirection(metric, delta) {
  if (!delta || typeof delta.before !== 'number' || typeof delta.after !== 'number') return 'none';
  if (delta.after === delta.before) return 'none';
  const up = delta.after > delta.before;
  const worse = metric.worse === 'up' ? up : !up;
  return worse ? 'worse' : 'better';
}

export const CHANGE_VERDICT = {
  regression: { tone: 'error', label: 'regression' },
  improvement: { tone: 'ok', label: 'improvement' },
  neutral: { tone: 'neutral', label: 'neutral' },
  insufficient_data: { tone: 'neutral', label: 'insufficient data' },
};

export const DIMENSION_LABEL = {
  system_prompt: 'system prompt',
  tool_catalog: 'tool catalog',
  model_requested: 'model requested',
  model_returned: 'model returned',
  params: 'parameters',
  service_version: 'service version',
  sdk_version: 'SDK version',
  behavior: 'behaviour',
};

/** A change's value as shown: digests stay mono and whole (16 hex), paths
    are cut to the width a timeline label has. */
export function changeValue(value, max = 64) {
  if (typeof value !== 'string') return '—';
  return value.length > max ? value.slice(0, max - 1) + '…' : value;
}

/** Where a change sits on its agent's timeline, 0–1, clamped. */
export function timelinePosition(atNs, firstNs, lastNs) {
  if (!(lastNs > firstNs)) return 0.5;
  return Math.min(1, Math.max(0, (atNs - firstNs) / (lastNs - firstNs)));
}

// -------------------------------------------------------------------- TQL

/** Worked examples from the TQL guide, each one query and what it finds. */
export const TQL_EXAMPLES = [
  { q: 'tool[error]', means: 'every failing tool call' },
  { q: 'test[error] ..> edit[file~"(^|/)tests?/"] ..> test[not error]', means: 'a failing test, an edit under tests/, then a pass' },
  { q: 'edit[file=@f] ..> edit[file=@f]', means: 'the same file edited twice' },
  { q: 'edit[file=@f] (..> edit[file=@f]){4,} within 10', means: 'one file edited five times inside ten steps' },
  { q: 'tool(@t)[error] (-> tool(@t)[error]){2,}', means: 'the same tool failing three times in a row' },
  { q: 'tool[in_catalog=false]', means: 'a tool the model was never offered' },
  { q: 'step[duration>30s]', means: 'a slow step' },
  { q: 'llm[cost>0.5]', means: 'an expensive model call' },
  { q: 'tool(bash)[args.command~"^rm -rf"]', means: 'a dangerous command, by argument' },
  { q: 'edit ..[not test]> llm$', means: 'finished without testing after the last edit' },
  { q: 'llm[context>=1000, context=@c] ..> llm[context>=2*@c] within 6', means: 'the context doubled within five steps' },
  { q: 'tool(search_orders){3,}', means: 'a search re-run three or more times in a row' },
  { q: 'step[error]$', means: 'the run ended on an error' },
];

/** The reference the query box shows: atoms, links and predicates, each a
    line of syntax and what it means — the guide's tables, condensed. */
export const TQL_SYNTAX = [
  { group: 'steps', rows: [
    ['step · tool · llm · agent', 'any step, a tool call, a model call, an agent invocation'],
    ['tool(bash) · tool(/^mcp__/)', 'by name (ASCII case ignored), or by regex'],
    ['edit · edit(PATH) · test', 'a file edit; a test run (heuristics, see the guide)'],
    ['tool(@t)', 'capture the name; later uses must match it'],
  ] },
  { group: 'links', rows: [
    ['A -> B', 'B is the very next step'],
    ['A ..> B', 'B comes later, any steps between'],
    ['A ..[kind=llm]> B', 'later, and every step between satisfies the predicate'],
    ['X+ · X{3,} · X?', 'repetition of adjacent matches'],
    ['^A · B$ · within 10 · within 5m', 'first step, last step, a bound on the match'],
  ] },
  { group: 'predicates', rows: [
    ['[error] · [not error]', 'status is error; is not'],
    ['[args~"pytest"] · [result~"(?i)denied"]', 'the input or output matches a regex'],
    ['[cost>0.5] · [duration>30s] · [tokens>20000]', 'numeric comparisons; durations need a unit'],
    ['[file=@f] · [args.command~"^rm"] · [attr.k=v]', 'a capture, a path into the input, any attribute'],
  ] },
];

/** A caret under the 1-based character `column` a refusal names, for a
    monospace echo of the query. Counts characters as the server does
    (code points), so a query with a multi-byte character still lines up. */
export function caretLine(query, column) {
  if (typeof column !== 'number' || column < 1) return '';
  const before = Array.from(String(query || '')).slice(0, column - 1);
  return before.map((ch) => (ch === '\t' ? '\t' : ' ')).join('') + '^';
}

/** The detectors' severities, in the order findings are ranked. */
export const SEVERITY = {
  high: { tone: 'error', rank: 0 },
  medium: { tone: 'warn', rank: 1 },
  low: { tone: 'neutral', rank: 2 },
};

/** A detector's success rate as evidence: the server's rate, its Wilson
    interval over the matched runs whose outcome is known (the rate's own
    denominator, not `runs_matched`), that count, and whether the interval
    lies wholly below the baseline — the only case the row calls worse. */
export function detectorRate(detector, baseline) {
  const o = (detector && detector.outcomes) || {};
  const n = (o.success || 0) + (o.failure || 0) + (o.abandoned || 0);
  const ci = wilson(o.success || 0, n);
  const rate = detector && typeof detector.success_rate === 'number' ? detector.success_rate : null;
  return { rate, ci, n, below: Boolean(ci && typeof baseline === 'number' && ci[1] < baseline) };
}

/** A match's run as a route to its tape at the first matched step. */
export function matchRoute(match) {
  const params = { step: String(match.first_step ?? (match.spans && match.spans[0] && match.spans[0].step_index) ?? '') };
  if (typeof match.tenant === 'string') params.tenant = match.tenant;
  if (!params.step) delete params.step;
  return { parts: ['run', match.run], params };
}

/** A count as "1 of 350". */
export function ofTotal(part, whole) {
  return `${fmtNum(part)} of ${fmtNum(whole)}`;
}
