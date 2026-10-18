// Mission Control's client: the live-runs stream, the rows it keeps, and the
// arithmetic the Live screen states about them.
//
// The stream (`GET /v1/runs/live/stream`) is server-sent events read with
// `fetch`, like the tail — `EventSource` cannot carry the bearer header, and
// a token in the URL lands in proxy logs and browser history. Its protocol is
// simpler than the tail's: an `event: snapshot` carrying the whole listing,
// then `event: run` with one run's row each time that run changes, and a
// `: tick` comment after fifteen quiet seconds. There is no resume cursor, so
// there is nothing to resume: every connection, first or reconnected, starts
// from a snapshot that replaces everything held. A break is therefore healed
// by construction, and what the client owes the reader is honesty about the
// time in between — the rows on screen are as of the break until the next
// snapshot lands.
//
// Kept free of the DOM, like lib/tail.js, so it can be driven by a test.

import { backoffMs, createFramer } from './tail.js';

/** How often the listing is read when the stream is refused. */
export const LIVE_POLL_MS = 5000;
/** How long a refused stream (409: the scope's eight streams are taken) is
    left before it is tried again; polling covers the time in between. */
export const STREAM_RETRY_MS = 30_000;
/** Rows kept at most. The snapshot holds 256; `run` frames add the runs that
    start after it, and the oldest by activity go first past this. */
export const ROW_CAP = 1000;
/** A run with no activity for this long reads as idle: it is still in the
    live window, but nothing has happened to it for a while — usually it has
    finished, which the live row cannot say. */
export const IDLE_MS = 30_000;
/** What the stream's snapshot holds, and what a poll asks for to match it. */
export const SNAPSHOT_LIMIT = 256;

/** The identity of a live row: a run id is unique within its tenant only. */
export function rowKey(row) {
  return (row.tenant || '') + '\u0000' + row.run;
}

/** Interprets one stream frame: a snapshot, a run, a tick, or null.

    A comment (`: tick`) is the heartbeat, returned so the runner can count
    it as progress. An unknown event is ignored, so the server can add frame
    types without breaking a client that predates them. */
export function parseLiveFrame(frame) {
  if (!frame) return null;
  if (frame.startsWith(':')) return { type: 'tick' };
  let event = 'message';
  const data = [];
  for (const line of frame.split('\n')) {
    if (line.startsWith('event:')) event = line.slice(6).trim();
    else if (line.startsWith('data:')) data.push(line.slice(5).replace(/^ /, ''));
  }
  if (!data.length) return null;
  let payload;
  try { payload = JSON.parse(data.join('\n')); } catch (e) { return null; }
  if (event === 'snapshot' && payload && typeof payload === 'object') {
    return {
      type: 'snapshot',
      runs: Array.isArray(payload.runs) ? payload.runs.filter(isRow) : [],
      truncated: Boolean(payload.truncated),
      windowNs: typeof payload.window_ns === 'number' ? payload.window_ns : null,
    };
  }
  if (event === 'run' && isRow(payload)) return { type: 'run', row: payload };
  return null;
}

function isRow(row) {
  return Boolean(row && typeof row === 'object' && typeof row.run === 'string');
}

// ------------------------------------------------------------------ state
//
// The rows are a Map from key to an entry: `{row, flashAt}`. `flashAt` is the
// client time the row last changed, so the screen can mark the change; a row
// re-sent unchanged keeps its old entry, and the first snapshot marks
// nothing (everything is new, so nothing is news).
//
// `offsetMs` is the estimated server clock minus the client's. The server's
// timestamps — last activity, a pending step's start — are its own clock,
// and the stream carries no "now". A `run` frame that moved a run's last
// activity arrives within about half a second of that activity (200 ms
// polling plus 250 ms coalescing), so its timestamp is a reading of the
// server's clock; the estimate follows those readings. Before the first,
// the clocks are assumed to agree, unless a row's activity is already in
// the client's future.

/** Fresh state: nothing held, the server's default window assumed. */
export function newLiveState() {
  return { rows: new Map(), windowNs: 900e9, truncated: false, snapshots: 0, offsetMs: null };
}

function sameRow(a, b) {
  return JSON.stringify(a) === JSON.stringify(b);
}

/** Replaces everything with a snapshot (or a polled listing). */
export function applySnapshot(state, snapshot, nowMs) {
  const rows = new Map();
  const first = state.snapshots === 0 && state.rows.size === 0;
  let offset = state.offsetMs;
  for (const row of snapshot.runs) {
    const key = rowKey(row);
    const old = state.rows.get(key);
    rows.set(key, old && sameRow(old.row, row) ? old : { row, flashAt: first ? 0 : nowMs });
    // Activity in the client's future proves the server's clock is ahead.
    const ahead = activityMs(row) - nowMs;
    if (ahead > 0 && (offset == null || ahead > offset)) offset = ahead;
  }
  return {
    rows: capRows(rows),
    windowNs: snapshot.windowNs || state.windowNs,
    truncated: snapshot.truncated,
    snapshots: state.snapshots + 1,
    offsetMs: offset,
  };
}

/** Upserts one run's row from an `event: run` frame. */
export function applyRun(state, row, nowMs) {
  const key = rowKey(row);
  const old = state.rows.get(key);
  if (old && sameRow(old.row, row)) return state;
  const rows = new Map(state.rows);
  rows.set(key, { row, flashAt: nowMs });
  let offset = state.offsetMs;
  // A moved last activity is a fresh reading of the server's clock.
  if (!old || activityMs(row) > activityMs(old.row)) {
    const reading = activityMs(row) - nowMs;
    offset = offset == null ? reading : offset * 0.5 + reading * 0.5;
  }
  return { ...state, rows: capRows(rows), offsetMs: offset };
}

function activityMs(row) {
  return typeof row.last_activity_ns === 'number' ? row.last_activity_ns / 1e6 : 0;
}

function capRows(rows) {
  if (rows.size <= ROW_CAP) return rows;
  const kept = [...rows.entries()].sort((a, b) => activityMs(b[1].row) - activityMs(a[1].row)).slice(0, ROW_CAP);
  return new Map(kept);
}

/** The server's clock now, by the estimate. */
export function serverNowMs(state, nowMs) {
  return nowMs + (state.offsetMs || 0);
}

/** Drops rows whose last activity has left the live window. The stream sends
    nothing when a run goes quiet, so the client ages rows out itself. A few
    seconds of grace keep a row from flickering out on clock noise. */
export function ageOut(state, nowMs) {
  const limitMs = state.windowNs / 1e6 + 5000;
  const now = serverNowMs(state, nowMs);
  let changed = false;
  const rows = new Map();
  for (const [key, entry] of state.rows) {
    if (now - activityMs(entry.row) > limitMs) { changed = true; continue; }
    rows.set(key, entry);
  }
  return changed ? { ...state, rows } : state;
}

/** Milliseconds since the run's last activity, by the server's clock. */
export function idleMs(row, serverNow) {
  return Math.max(0, serverNow - activityMs(row));
}

/** Whether a run has gone quiet: no activity for `IDLE_MS`, and no step
    pending. A run waiting on a step it named — a long tool, or a check that
    answered stop and is holding it — is not quiet, however long since its
    last span: it is the run most likely to want a person. */
export function isQuiet(row, serverNow) {
  return idleMs(row, serverNow) > IDLE_MS && !(row.current && row.current.pending);
}

/** Milliseconds the current step has been running (or pending). A pending
    step's start is the server's clock; a recorded step's is the span's. */
export function stepElapsedMs(row, serverNow) {
  const start = row.current && typeof row.current.started_ns === 'number' ? row.current.started_ns / 1e6 : null;
  return start == null ? null : Math.max(0, serverNow - start);
}

// ------------------------------------------------------------- reading rows

/** The guard's verdict as the screen shows it: a run held by a predictive
    stop (found by a verdict read; the listing is from memory and never
    carries one) counts as stopped. */
export function guardState(entry) {
  if (entry.row.verdict === 'stop') return 'stop';
  if (entry.held) return 'held';
  return entry.row.verdict || 'continue';
}

/** How urgently a run wants a person, lowest first: stopped or held, then
    foresight's likely failures and at-risk runs, guard warnings, novel
    paths, runs with nudges waiting, then everything else. */
export function riskRank(entry) {
  const row = entry.row;
  if (row.verdict === 'stop' || entry.held) return 0;
  const verdict = row.foresight && row.foresight.verdict;
  if (verdict === 'likely_failure') return 1;
  if (verdict === 'at_risk') return 2;
  if (row.verdict === 'warn') return 3;
  if (verdict === 'novel') return 4;
  if (row.pending_nudges > 0) return 5;
  return 6;
}

/** Whether a run is in one of the states someone should look at. */
export function needsAttention(entry) {
  return riskRank(entry) <= 4;
}

export const SORTS = [
  { id: 'risk', label: 'risk' },
  { id: 'recent', label: 'recent' },
  { id: 'cost', label: 'cost' },
];

/** Orders entries for display.

    `risk` puts active runs before quiet ones, then sorts by rank, and breaks
    ties by start time — which never changes — so a row moves only when its
    risk does. Sorting a live table by anything that ticks makes rows jump
    out from under the pointer; `recent` does exactly that, on request. */
export function sortEntries(entries, sort, serverNow) {
  const list = [...entries];
  const started = (entry) => entry.row.started_ns || 0;
  const tie = (a, b) => started(b) - started(a) || (rowKey(a.row) < rowKey(b.row) ? -1 : 1);
  if (sort === 'recent') return list.sort((a, b) => activityMs(b.row) - activityMs(a.row) || tie(a, b));
  if (sort === 'cost') return list.sort((a, b) => (b.row.cost_usd || 0) - (a.row.cost_usd || 0) || tie(a, b));
  return list.sort((a, b) => {
    const idleA = isQuiet(a.row, serverNow) ? 1 : 0;
    const idleB = isQuiet(b.row, serverNow) ? 1 : 0;
    return idleA - idleB || riskRank(a) - riskRank(b) || tie(a, b);
  });
}

export const FILTERS = [
  { id: 'all', label: 'all' },
  { id: 'attention', label: 'needs attention' },
  { id: 'active', label: 'active' },
  { id: 'stopped', label: 'stopped' },
  { id: 'nudged', label: 'nudges waiting' },
];

/** Whether an entry passes a filter and a search. */
export function matches(entry, { filter = 'all', agent = '', search = '' }, serverNow) {
  const row = entry.row;
  if (agent && row.agent !== agent) return false;
  if (search) {
    const needle = search.toLowerCase();
    const hay = [row.run, row.agent, row.service, row.current && row.current.name].filter(Boolean).join(' ').toLowerCase();
    if (!hay.includes(needle)) return false;
  }
  switch (filter) {
    case 'attention': return needsAttention(entry);
    case 'active': return !isQuiet(row, serverNow);
    case 'stopped': return guardState(entry) === 'stop' || guardState(entry) === 'held';
    case 'nudged': return row.pending_nudges > 0;
    default: return true;
  }
}

/** The figures across the live runs: what the summary strip states. Money
    and tokens are sums of what the guard counted per run. */
export function liveTotals(entries, serverNow) {
  const totals = { live: 0, active: 0, cost: 0, tokens: 0, atRisk: 0, stopped: 0, nudges: 0, attention: 0 };
  for (const entry of entries) {
    const row = entry.row;
    totals.live += 1;
    if (!isQuiet(row, serverNow)) totals.active += 1;
    if (typeof row.cost_usd === 'number') totals.cost += row.cost_usd;
    if (typeof row.tokens === 'number') totals.tokens += row.tokens;
    const verdict = row.foresight && row.foresight.verdict;
    if (verdict === 'likely_failure' || verdict === 'at_risk') totals.atRisk += 1;
    const guard = guardState(entry);
    if (guard === 'stop' || guard === 'held') totals.stopped += 1;
    if (row.pending_nudges > 0) totals.nudges += row.pending_nudges;
    if (needsAttention(entry)) totals.attention += 1;
  }
  return totals;
}

/** The agents among the rows, most runs first: the agent filter's choices. */
export function agentsOf(entries) {
  const counts = new Map();
  for (const entry of entries) counts.set(entry.row.agent, (counts.get(entry.row.agent) || 0) + 1);
  return [...counts.entries()].sort((a, b) => b[1] - a[1] || (a[0] < b[0] ? -1 : 1)).map(([agent, runs]) => ({ agent, runs }));
}

// --------------------------------------------------------------- foresight

export const FORESIGHT = {
  on_track: { label: 'on track', tag: 'ok' },
  at_risk: { label: 'at risk', tag: 'warn' },
  likely_failure: { label: 'likely failure', tag: 'error' },
  novel: { label: 'novel path', tag: 'neutral' },
  insufficient_history: { label: 'no history', tag: 'neutral' },
};

const clamp01 = (value) => Math.min(1, Math.max(0, value));

/** Where a foresight's marks fall on a 0–100% track: the success rate, its
    95% interval, and the agent's base rate. Each is null when the server
    gave none — an agent with no completed runs has no base rate, and a run
    with no support no interval. */
export function foresightMarks(foresight) {
  if (!foresight) return null;
  const num = (value) => (typeof value === 'number' && Number.isFinite(value) ? clamp01(value) : null);
  const ci = Array.isArray(foresight.ci95) && foresight.ci95.length === 2 ? foresight.ci95 : null;
  return {
    p: num(foresight.p_success),
    lo: ci ? num(ci[0]) : null,
    hi: ci ? num(ci[1]) : null,
    base: num(foresight.base_rate),
    support: typeof foresight.support === 'number' ? foresight.support : 0,
  };
}

/** A rate as a whole percent, with one decimal under ten percent so 4.3%
    does not read as 4%. */
export function fmtPct(value) {
  if (typeof value !== 'number' || !Number.isFinite(value)) return '—';
  const pct = value * 100;
  if (pct > 0 && pct < 10) return pct.toFixed(1) + '%';
  return Math.round(pct) + '%';
}

/** "4.3–21%": an interval, compactly. */
export function fmtInterval(ci) {
  if (!Array.isArray(ci) || ci.length !== 2) return '';
  return `${fmtPct(ci[0]).replace('%', '')}–${fmtPct(ci[1])}`;
}

/** The sentence a foresight stands for, built from the row's own figures
    (the live row carries no explanation; the run route does). */
export function foresightSentence(foresight) {
  if (!foresight) return 'No foresight yet: the server holds no whole path for this run.';
  const { verdict, support, p_success: p, base_rate: base } = foresight;
  if (verdict === 'insufficient_history') {
    return support
      ? `Only ${support} earlier runs of this agent took this path; a verdict needs 30.`
      : 'No earlier run of this agent took this path yet, or no map is built for it yet.';
  }
  const rate = `runs that took this path succeeded ${fmtPct(p)} of the time (95% CI ${fmtInterval(foresight.ci95)}, ${support} runs)`;
  const against = typeof base === 'number' ? `; the agent's overall rate is ${fmtPct(base)}` : '';
  if (verdict === 'novel') return `This run took a step no earlier run of its agent took at that point. Before it, ${rate}${against}.`;
  return rate.charAt(0).toUpperCase() + rate.slice(1) + against + '.';
}

// ------------------------------------------------------------------ nudges

export const NUDGE_MAX_BYTES = 4096;

export const NUDGE_TTLS = [
  { id: '600', label: '10 min', seconds: 600 },
  { id: '3600', label: '1 hour', seconds: 3600 },
  { id: '86400', label: '1 day', seconds: 86400 },
  { id: '604800', label: '7 days', seconds: 604800 },
];

/** A message's size as the server counts it: UTF-8 bytes, not characters. */
export function utf8Bytes(text) {
  return new TextEncoder().encode(text || '').length;
}

/** Why a nudge cannot be sent, or '' when it can. */
export function nudgeProblem(message) {
  const bytes = utf8Bytes(message);
  if (!message || !message.trim()) return 'Write the message the agent should hear.';
  if (bytes > NUDGE_MAX_BYTES) return `The message is ${bytes.toLocaleString('en-US')} bytes; the server takes at most 4,096.`;
  return '';
}

/** How long a delivered nudge waited for its check, in milliseconds. */
export function deliveryWaitMs(nudge) {
  if (!nudge || !nudge.delivered_at_ns || !nudge.created_at_ns) return null;
  return Math.max(0, (nudge.delivered_at_ns - nudge.created_at_ns) / 1e6);
}

// ------------------------------------------------------------------ stream

const defaultSleep = (ms) => new Promise((done) => setTimeout(done, ms));

/** What a failure means for the stream: `denied` (401/403 — nothing will
    change until the credential does), `missing` (404: a server that
    predates the live view), `disabled` (409 from a store started with
    --no-fuses), `refused` (409: every stream slot of the scope is taken —
    poll instead), or `broken` (anything else: reconnect). */
export function classifyFailure(error) {
  const status = error && error.status;
  if (status === 401 || status === 403) return 'denied';
  if (status === 404) return 'missing';
  if (status === 409) return /fuses/.test((error && error.what) || '') && /disabled/.test(error.what || '') ? 'disabled' : 'refused';
  return 'broken';
}

/** Follows the live view until `signal` aborts.

    `open(params)` resolves an async iterable of text chunks (the stream);
    `poll(params)` resolves a `GET /v1/runs/live` document. Both are
    injected, so a test drives this with generators rather than sockets.

    `onSnapshot(snapshot, {source})` replaces everything; `onRun(row)`
    upserts one row; `onStatus({state, …})` reports `live`, `reconnecting`
    (with `inMs`), `polling` (with the refusal), `disabled`, `missing` or
    `denied`.

    Backoff resets on progress — a frame, the heartbeat included — never on
    a connection merely being accepted, for the reason lib/tail.js gives: a
    server that accepts and at once closes must not be reconnected to every
    half second forever. */
export async function runLiveStream(options) {
  const {
    open, poll, params = {}, onSnapshot, onRun, onStatus = () => {}, signal,
    sleep = defaultSleep, now = Date.now,
  } = options;
  const aborted = () => Boolean(signal && signal.aborted);
  let attempt = 0;
  let pollUntil = 0;
  let refusal = null;

  while (!aborted()) {
    if (now() < pollUntil) {
      try {
        const doc = await poll({ ...params, limit: SNAPSHOT_LIMIT });
        if (aborted()) return;
        onSnapshot({
          runs: (doc && Array.isArray(doc.runs) ? doc.runs : []).filter(isRow),
          truncated: Boolean(doc && doc.truncated),
          windowNs: doc && typeof doc.window_ns === 'number' ? doc.window_ns : null,
        }, { source: 'poll' });
        onStatus({ state: 'polling', error: refusal });
      } catch (error) {
        if (aborted()) return;
        const kind = classifyFailure(error);
        if (kind === 'denied' || kind === 'disabled' || kind === 'missing') { onStatus({ state: kind, error }); return; }
      }
      await sleep(LIVE_POLL_MS);
      continue;
    }

    try {
      const chunks = await open(params);
      const framer = createFramer();
      for await (const chunk of chunks) {
        if (aborted()) return;
        for (const raw of framer(chunk)) {
          attempt = 0;
          const frame = parseLiveFrame(raw);
          if (!frame) continue;
          if (frame.type === 'snapshot') {
            onSnapshot(frame, { source: 'stream' });
            onStatus({ state: 'live' });
          } else if (frame.type === 'run') {
            onRun(frame.row);
          }
        }
      }
    } catch (error) {
      if (aborted()) return;
      const kind = classifyFailure(error);
      if (kind === 'denied' || kind === 'disabled' || kind === 'missing') { onStatus({ state: kind, error }); return; }
      if (kind === 'refused') {
        refusal = error;
        pollUntil = now() + STREAM_RETRY_MS;
        onStatus({ state: 'polling', error });
        continue;
      }
    }
    if (aborted()) return;
    const wait = backoffMs(attempt);
    onStatus({ state: 'reconnecting', inMs: wait });
    await sleep(wait);
    attempt += 1;
  }
}
