// The query a screen is asking, as data.
//
// Filters used to live in component state, which meant the search that found
// the bug could not be sent to anyone. A query is now a value: it serializes
// into the hash route, out of it, into API parameters, and into the curl
// command that reproduces it. Everything downstream — sharing, saved views,
// "copy as curl", the export that matches what is on screen — falls out of
// that one representation rather than being built per screen.

/** Predicate operators, in the order the builder offers them. `≠` is spelled
    out because its semantics are surprising: a span that never recorded the
    key is KEPT, which the UI has to say rather than imply. */
export const OPS = ['=', '≠', '≥', '≤'];

/** Fields that are span columns rather than attributes. */
export const FIELDS = ['service', 'name', 'status', 'session', 'duration_ms'];

/** Which operators each field actually supports. */
export function opsFor(field) {
  if (field === 'duration_ms') return ['≥', '≤'];
  if (field === 'service' || field === 'name' || field === 'session') return ['='];
  if (field === 'status') return ['=', '≠'];
  return OPS; // attr.*
}

let nextId = 1;
/** A predicate carries an id so React can key rows across reordering without
    remounting the input the user is typing in. */
export function predicate(field = 'service', op = '=', value = '') {
  return { id: nextId++, field, op, value };
}

/** Relative windows, newest-first as the mockup orders them. `all` omits the
    bounds entirely so the server never prunes by time. */
export const RANGES = [
  { id: '15m', label: '15m', ms: 15 * 60e3 },
  { id: '1h', label: '1h', ms: 3600e3 },
  { id: '24h', label: '24h', ms: 86400e3 },
  { id: '7d', label: '7d', ms: 7 * 86400e3 },
  { id: 'all', label: 'all', ms: null },
];

/** Resolves a range id (or an absolute pair) to nanosecond bounds.

    Relative windows resolve against `now` at call time rather than being
    frozen into the query: a shared link to "the last hour" should mean the
    recipient's last hour, not the sender's. */
export function windowOf(range, now = Date.now()) {
  if (range && range.sinceNs && range.untilNs) {
    return { sinceNs: range.sinceNs, untilNs: range.untilNs };
  }
  const id = typeof range === 'string' ? range : (range && range.id) || '1h';
  const found = RANGES.find((r) => r.id === id);
  if (!found || found.ms == null) return { sinceNs: null, untilNs: null };
  return { sinceNs: (now - found.ms) * 1e6, untilNs: now * 1e6 };
}

/** An empty query: no text, no predicates, last hour, newest first.

    `content` is a field rather than a predicate on purpose. It is the one
    filter somebody reaches for without already knowing the schema, so it gets
    its own full-width control above the builder instead of being one row among
    several — and it is word matching, not a substring or a phrase, which no
    predicate operator would say correctly. */
export function emptyQuery() {
  return { content: '', preds: [], range: '1h', sort: '', limit: 100 };
}

/** Turns predicates into API parameters.

    Repeated keys become arrays, which the client's query builder expands into
    repeated parameters — that is how the API expresses two conditions on one
    attribute, and the old single-pair form could not say it at all. */
export function toParams(q, { includeWindow = true, extra = {} } = {}) {
  const params = {};
  if (q.content && q.content.trim()) params.content = q.content.trim();
  const push = (key, value) => {
    if (value === '' || value == null) return;
    if (params[key] === undefined) params[key] = value;
    else if (Array.isArray(params[key])) params[key].push(value);
    else params[key] = [params[key], value];
  };
  for (const p of q.preds || []) {
    const value = String(p.value ?? '').trim();
    if (!value && p.field !== 'status') continue;
    if (p.field === 'duration_ms') {
      push(p.op === '≤' ? 'max_duration_ms' : 'min_duration_ms', value);
      continue;
    }
    if (p.field === 'status') {
      push(p.op === '≠' ? 'not_status' : 'status', value);
      continue;
    }
    if (p.field === 'service' || p.field === 'name' || p.field === 'session') {
      push(p.field, value);
      continue;
    }
    // Everything else is an attribute path; the operator picks the family.
    const key = p.field.replace(/^attr\./, '');
    if (p.op === '≠') push('not_attr.' + key, value);
    else if (p.op === '≥') push('min_attr.' + key, value);
    else if (p.op === '≤') push('max_attr.' + key, value);
    else push('attr.' + key, value);
  }
  if (includeWindow) {
    const { sinceNs, untilNs } = windowOf(q.range);
    if (sinceNs) params.since = Math.round(sinceNs);
    if (untilNs) params.until = Math.round(untilNs);
  }
  if (q.sort) params.sort = q.sort;
  if (q.limit) params.limit = q.limit;
  return { ...params, ...extra };
}

// A predicate serializes as `field|op|value`, joined by `~`. The separators
// are percent-encoded out of the parts first, so a value containing either is
// carried literally rather than splitting the predicate in half.
const P_SEP = '~';
const F_SEP = '|';
const escape = (text) => String(text).replace(/[~|%]/g, (c) => '%' + c.charCodeAt(0).toString(16));
const unescape = (text) => String(text).replace(/%([0-9a-f]{2})/gi, (_, hex) => String.fromCharCode(parseInt(hex, 16)));

/** Serializes a query into hash-route parameters. */
export function toHash(q) {
  const out = {};
  // `c`, not `q`: the hash already spends `q` on the predicate list, and the
  // API spends it as its own alias for content. Separate namespaces, but one
  // letter meaning two things in the same URL is a trap for the next reader.
  if (q.content && q.content.trim()) out.c = q.content.trim();
  if (q.preds && q.preds.length) {
    out.q = q.preds
      .filter((p) => String(p.value ?? '') !== '' || p.field === 'status')
      .map((p) => [escape(p.field), escape(p.op), escape(p.value)].join(F_SEP))
      .join(P_SEP);
  }
  // A range is either a preset id or an absolute pair. Both have to survive
  // the URL: the volume brush produces an absolute one, and serializing it as
  // an empty string — which is what `typeof range === 'string' ? … : ''` did —
  // silently threw the drag away and restored the default window.
  if (q.range && q.range !== '1h') {
    if (typeof q.range === 'string') out.t = q.range;
    else if (q.range.sinceNs && q.range.untilNs) {
      out.t = `${Math.round(q.range.sinceNs)}${ABS_SEP}${Math.round(q.range.untilNs)}`;
    }
  }
  if (q.sort) out.s = q.sort;
  if (q.limit && q.limit !== 100) out.n = String(q.limit);
  return out;
}

/** Separates the two halves of an absolute window in the hash. A dash cannot
    appear in a decimal nanosecond timestamp, so it needs no escaping. */
const ABS_SEP = '-';

/** Reads a `t=` value: a preset id, or `<sinceNs>-<untilNs>`. */
function parseRange(raw) {
  if (!raw) return null;
  const at = raw.indexOf(ABS_SEP);
  if (at > 0) {
    const sinceNs = Number(raw.slice(0, at));
    const untilNs = Number(raw.slice(at + 1));
    // Both halves must be finite and ordered, or this is not a window —
    // treat a mangled one as absent rather than as a range of nonsense.
    if (Number.isFinite(sinceNs) && Number.isFinite(untilNs) && untilNs > sinceNs) {
      return { sinceNs, untilNs };
    }
    return null;
  }
  return RANGES.some((r) => r.id === raw) ? raw : null;
}

/** Reads a query back out of hash-route parameters. */
export function fromHash(params) {
  const q = emptyQuery();
  if (params.get('c')) q.content = params.get('c');
  const raw = params.get('q');
  if (raw) {
    q.preds = raw.split(P_SEP).filter(Boolean).map((chunk) => {
      const [field, op, ...rest] = chunk.split(F_SEP);
      return predicate(unescape(field || 'service'), unescape(op || '='), unescape(rest.join(F_SEP) || ''));
    });
  }
  const range = parseRange(params.get('t'));
  if (range) q.range = range;
  if (params.get('s')) q.sort = params.get('s');
  if (params.get('n')) q.limit = Number(params.get('n')) || 100;
  return q;
}

/** Two queries are equal when they'd send the same request. */
export function sameQuery(a, b) {
  return JSON.stringify(toParams(a)) === JSON.stringify(toParams(b));
}

/** The curl command that reproduces this query, for handing to someone whose
    dashboard is a terminal. */
export function toCurl(q, origin, path = '/v1/spans') {
  const params = toParams(q);
  const pairs = [];
  for (const [key, value] of Object.entries(params)) {
    for (const one of Array.isArray(value) ? value : [value]) {
      pairs.push(encodeURIComponent(key) + '=' + encodeURIComponent(one));
    }
  }
  const auth = " -H 'Authorization: Bearer $TRAZA_TOKEN'";
  return `curl${auth} '${origin}${path}${pairs.length ? '?' + pairs.join('&') : ''}'`;
}

/** A short human label for a predicate, used on chips and in saved views. */
export function describe(p) {
  const field = p.field === 'duration_ms' ? 'duration' : p.field;
  const unit = p.field === 'duration_ms' ? ' ms' : '';
  return `${field} ${p.op} ${p.value}${unit}`;
}
