// The Live screen's client logic, driven by what a real 0.28.0 server sent:
// `live_stream.txt` is GET /v1/runs/live/stream as captured (a snapshot and
// four `run` frames; the snapshot cut to the runs `live_runs.json` keeps),
// and `live_runs.json` is GET /v1/runs/live from the same server, with
// simulated agents checking before each step under a foresight policy.

import { describe, it, expect, vi } from 'vitest';
import streamText from '../test/fixtures/live_stream.txt?raw';
import listing from '../test/fixtures/live_runs.json';
// A support-app run from the same server minutes earlier, before its agent
// had a completed run: no base rate, no interval, no support.
import noHistory from '../test/fixtures/live_row_no_history.json';
// GET /v1/runs/live/stream from a server run with `--fuse-max-runs 1
// --live-window-seconds 3`: a second run took the guard's one slot (the
// first is `evicted`), then went quiet past the window (`expired`).
import removedText from '../test/fixtures/live_stream_removed.txt?raw';
// GET /v1/runs/live/stream from a server holding 257 live runs, the first
// posted 30 s before the rest: the snapshot left that one out (`truncated`
// as sent; its rows cut here to the first three of 256), and a nudge queued
// for it then sent its row in a `run` frame — the row changed, its last
// activity did not.
import unseenText from '../test/fixtures/live_stream_unseen.txt?raw';
// GET /v1/runs/live?limit=3 from the same server just after: the nudged
// run, most recently changed, first.
import afterNudge from '../test/fixtures/live_runs_after_nudge.json';
import { createFramer } from './tail.js';
import {
  DEFERRED_SENTENCE, IDLE_MS, LIVE_POLL_MS, ROW_CAP, STALL_MS, STREAM_RETRY_MS, WAITING_MS, ageOut, agentsOf, applyRemoved,
  applyRun, applySnapshot, classifyFailure, deliveryWaitMs, fmtInterval, fmtPct, foresightMarks, foresightSentence, guardState, isPartial,
  isQuiet, isWaiting, keepListed, liveTotals, matches, needsAttention, newLiveState, nudgeProblem, parseLiveFrame, probeCandidates,
  riskRank, rowKey, runLiveStream, serverNowMs, sortEntries, utf8Bytes,
} from './live.js';

const frames = () => createFramer()(streamText).map(parseLiveFrame).filter(Boolean);
const byRun = (run) => listing.runs.find((row) => row.run === run);
/** The server's clock at the capture: the newest activity in the listing. */
const CAPTURE_MS = Math.max(...listing.runs.map((row) => row.last_activity_ns)) / 1e6;

describe('the stream as the server sent it', () => {
  it('frames into one snapshot, then a row per changed run', () => {
    const parsed = frames();
    expect(parsed.map((f) => f.type)).toEqual(['snapshot', 'run', 'run', 'run', 'run']);
    const [snapshot, ...runs] = parsed;
    expect(snapshot.windowNs).toBe(900e9);
    expect(snapshot.truncated).toBe(false);
    expect(snapshot.runs.map((row) => row.run)).toContain('tkt-4162');
    for (const frame of runs) expect(typeof frame.row.run).toBe('string');
  });

  it('reads the heartbeat as progress, and ignores events it does not know', () => {
    expect(parseLiveFrame(': tick')).toEqual({ type: 'tick' });
    expect(parseLiveFrame('event: future\ndata: {"run":"x"}')).toBeNull();
    expect(parseLiveFrame('event: run\ndata: {not json')).toBeNull();
    expect(parseLiveFrame('event: run\ndata: {"no_run":1}')).toBeNull();
  });

  it('reads a removal: the run, its tenant and why it left the view', () => {
    const parsed = createFramer()(removedText).map(parseLiveFrame).filter(Boolean);
    expect(parsed.map((f) => f.type)).toEqual(['snapshot', 'removed', 'run', 'removed']);
    expect(parsed[1]).toEqual({ type: 'removed', run: 'tkt-5001', tenant: '', reason: 'evicted' });
    expect(parsed[3]).toEqual({ type: 'removed', run: 'tkt-5002', tenant: '', reason: 'expired' });
    expect(parseLiveFrame('event: removed\ndata: {"reason":"evicted"}')).toBeNull();
  });

  it('survives a frame split anywhere, multi-byte names included', () => {
    const framer = createFramer();
    const text = 'event: run\ndata: {"run":"größe-1","tenant":""}\n\n';
    const out = [];
    for (let i = 0; i < text.length; i += 3) out.push(...framer(text.slice(i, i + 3)));
    expect(out.map(parseLiveFrame)).toEqual([{ type: 'run', row: { run: 'größe-1', tenant: '' } }]);
  });
});

describe('the rows', () => {
  it('a snapshot replaces everything, and the first marks nothing as changed', () => {
    const [snapshot] = frames();
    const state = applySnapshot(newLiveState(), snapshot, 1000);
    expect(state.rows.size).toBe(snapshot.runs.length);
    expect([...state.rows.values()].every((entry) => entry.flashAt === 0)).toBe(true);
    // A later snapshot replaces the set: runs it does not carry are gone.
    const next = applySnapshot(state, { runs: [snapshot.runs[0]], truncated: false, windowNs: 900e9 }, 2000);
    expect([...next.rows.keys()]).toEqual([rowKey(snapshot.runs[0])]);
    // Unchanged rows keep their entry; nothing flashes for being re-sent.
    expect(next.rows.get(rowKey(snapshot.runs[0]))).toBe(state.rows.get(rowKey(snapshot.runs[0])));
  });

  it('a run frame upserts one row and marks it changed; an identical one changes nothing', () => {
    const [snapshot, first] = frames();
    const state = applySnapshot(newLiveState(), snapshot, 1000);
    const next = applyRun(state, first.row, 5000);
    expect(next.rows.get(rowKey(first.row)).row).toEqual(first.row);
    expect(next.rows.get(rowKey(first.row)).flashAt).toBe(5000);
    expect(applyRun(next, first.row, 6000)).toBe(next);
  });

  it('a removal drops exactly the run it names, in its tenant', () => {
    let state = newLiveState();
    for (const frame of createFramer()(removedText).map(parseLiveFrame).filter(Boolean)) {
      if (frame.type === 'snapshot') state = applySnapshot(state, frame, 1000);
      else if (frame.type === 'run') state = applyRun(state, frame.row, 2000);
      else if (frame.type === 'removed') state = applyRemoved(state, frame);
      if (frame.type === 'removed') expect(state.rows.has(rowKey(frame))).toBe(false);
    }
    // Every event applied: the client holds what the listing holds, nothing.
    expect(state.rows.size).toBe(0);
    const row = byRun('tkt-4173');
    const two = applyRun(applyRun(newLiveState(), row, 1), { ...row, tenant: 'acme' }, 2);
    const one = applyRemoved(two, { run: row.run, tenant: 'acme', reason: 'evicted' });
    expect([...one.rows.keys()]).toEqual([rowKey(row)]);
    // A removal of a run not held changes nothing.
    expect(applyRemoved(one, { run: 'nobody', tenant: '', reason: 'expired' })).toBe(one);
  });

  it('keys a run by tenant and id: one id in two tenants is two runs', () => {
    const row = byRun('tkt-4173');
    const state = applyRun(applyRun(newLiveState(), row, 1), { ...row, tenant: 'acme' }, 2);
    expect(state.rows.size).toBe(2);
  });

  it('estimates the server clock from activity it has just been told about', () => {
    const row = byRun('tkt-4173');
    const activity = row.last_activity_ns / 1e6;
    // The client clock runs 60 s behind the server's.
    const state = applyRun(newLiveState(), row, activity - 60_000);
    expect(serverNowMs(state, activity - 60_000)).toBeCloseTo(activity, -1);
  });

  it('ages out a run whose last activity left the window, the stream sending nothing for it', () => {
    const [snapshot] = frames();
    let state = applySnapshot(newLiveState(), snapshot, CAPTURE_MS);
    state = ageOut(state, CAPTURE_MS + 10_000);
    expect(state.rows.size).toBe(snapshot.runs.length);
    state = ageOut(state, CAPTURE_MS + 960_000);
    expect(state.rows.size).toBe(0);
  });

  it('holds at most ROW_CAP rows, keeping the most recently active', () => {
    let state = newLiveState();
    for (let i = 0; i < ROW_CAP + 5; i += 1) {
      state = applyRun(state, { run: 'r' + i, tenant: '', last_activity_ns: (1e9 + i) * 1e6 }, 1e9 + i);
    }
    expect(state.rows.size).toBe(ROW_CAP);
    expect(state.rows.has(rowKey({ run: 'r0', tenant: '' }))).toBe(false);
  });
});

describe('what the screen states about a run', () => {
  const entry = (run, extra = {}) => ({ row: byRun(run), flashAt: 0, ...extra });

  it('ranks a stopped run, then foresight\'s likely failures, then the ordinary', () => {
    expect(riskRank(entry('conv-82febb'))).toBe(0); // the guard stopped it
    expect(riskRank(entry('tkt-4162'))).toBe(1); // likely failure
    expect(riskRank(entry('tkt-4173'))).toBe(7); // on track: the last rank
    expect(riskRank(entry('tkt-4173', { held: { doc: { verdict: 'stop' } } }))).toBe(0);
  });

  it('calls a run held when a verdict read found a stop the listing cannot carry', () => {
    expect(guardState(entry('tkt-4162'))).toBe('continue');
    expect(guardState(entry('tkt-4162', { held: { doc: { verdict: 'stop', reasons: ['foresight: …'] } } }))).toBe('held');
    expect(guardState(entry('conv-82febb'))).toBe('stop');
  });

  it('never calls a run waiting on a step it named quiet, however long since its last span', () => {
    const pending = byRun('tkt-4162');
    expect(pending.current.pending).toBe(true);
    expect(isQuiet(pending, pending.last_activity_ns / 1e6 + 5 * IDLE_MS)).toBe(false);
    const done = byRun('tkt-4142');
    expect(done.current.pending).toBe(false);
    expect(isQuiet(done, done.last_activity_ns / 1e6 + IDLE_MS + 1)).toBe(true);
  });

  it('sorts active runs by risk, ties by start, quiet ones after', () => {
    const entries = listing.runs.map((row) => ({ row, flashAt: 0 }));
    const sorted = sortEntries(entries, 'risk', CAPTURE_MS).map((e) => e.row.run);
    // The guard's stops lead; tkt-4142 (finished, quiet) trails.
    expect(['conv-82febb', 'conv-872843', 'conv-6ceeae']).toContain(sorted[0]);
    expect(sorted.indexOf('tkt-4162')).toBeLessThan(sorted.indexOf('tkt-4173'));
    expect(sorted[sorted.length - 1]).toBe('tkt-4142');
    expect(sortEntries(entries, 'cost', CAPTURE_MS)[0].row.cost_usd)
      .toBe(Math.max(...listing.runs.map((row) => row.cost_usd)));
  });

  it('filters by state, agent and a search over run, agent and step', () => {
    const entries = listing.runs.map((row) => ({ row, flashAt: 0 }));
    const names = (opts) => entries.filter((e) => matches(e, opts, CAPTURE_MS)).map((e) => e.row.run);
    expect(names({ filter: 'stopped' }).every((run) => run.startsWith('conv-'))).toBe(true);
    expect(names({ filter: 'nudged' })).toEqual(['tkt-4162']);
    expect(names({ agent: 'triage' }).every((run) => run.startsWith('conv-'))).toBe(true);
    expect(names({ search: 'search_orders' })).toEqual(expect.arrayContaining(['tkt-4162', 'tkt-4142']));
  });

  it('totals the runs: spend and tokens summed from the guard\'s figures', () => {
    const entries = listing.runs.map((row) => ({ row, flashAt: 0 }));
    const totals = liveTotals(entries, CAPTURE_MS);
    expect(totals.live).toBe(listing.runs.length);
    expect(totals.cost).toBeCloseTo(listing.runs.reduce((sum, row) => sum + row.cost_usd, 0), 10);
    expect(totals.tokens).toBe(listing.runs.reduce((sum, row) => sum + row.tokens, 0));
    expect(totals.nudges).toBe(1);
    expect(totals.stopped).toBe(listing.runs.filter((row) => row.verdict === 'stop').length);
    expect(agentsOf(entries)[0]).toEqual({ agent: 'support-agent', runs: listing.runs.filter((r) => r.agent === 'support-agent').length });
  });
});

describe('foresight', () => {
  it('places the rate, its interval and the base rate on the track', () => {
    const marks = foresightMarks(byRun('tkt-4162').foresight);
    expect(marks).toEqual({ p: 0.1, lo: 0.0466, hi: 0.2015, base: 0.5611, support: 60 });
    // An agent with no completed runs has no base rate and no interval.
    expect(foresightMarks(noHistory.foresight)).toEqual({ p: null, lo: null, hi: null, base: null, support: 0 });
    expect(foresightMarks(byRun('conv-82febb').foresight)).toEqual({ p: 0, lo: 0, hi: 0.3903, base: 0.5714, support: 6 });
  });

  it('says what the numbers mean, from the row\'s own figures', () => {
    expect(fmtPct(0.0466)).toBe('4.7%');
    expect(fmtPct(0.5611)).toBe('56%');
    expect(fmtInterval([0.0466, 0.2015])).toBe('4.7–20%');
    expect(foresightSentence(byRun('tkt-4162').foresight))
      .toBe("Runs that took this path succeeded 10% of the time (95% CI 4.7–20%, 60 runs); the agent's overall rate is 56%.");
    expect(foresightSentence(byRun('conv-82febb').foresight)).toBe('Only 6 earlier runs of this agent took this path; a verdict needs 30.');
    expect(foresightSentence(noHistory.foresight)).toMatch(/No earlier run of this agent took this path yet/);
    expect(foresightSentence(byRun('conv-c36f2e').foresight)).toMatch(/^This run took a step no earlier run of its agent took at that point/);
    expect(foresightSentence(undefined)).toMatch(/no whole path/);
    // A row the listing's one query budget did not reach says so, rather
    // than that there is nothing to say.
    expect(foresightSentence(undefined, true)).toBe(DEFERRED_SENTENCE);
    expect(foresightSentence(byRun('conv-82febb').foresight, true)).toMatch(/^Only 6 earlier runs/);
  });
});

describe('nudges', () => {
  it('counts the message in UTF-8 bytes, as the server does', () => {
    expect(utf8Bytes('größe')).toBe(7);
    expect(nudgeProblem('')).toMatch(/Write the message/);
    expect(nudgeProblem('   ')).toMatch(/Write the message/);
    expect(nudgeProblem('é'.repeat(2049))).toMatch(/4,098 bytes/);
    expect(nudgeProblem('Stop searching.')).toBe('');
  });

  it('measures how long a delivered nudge waited for its check', () => {
    expect(deliveryWaitMs({ created_at_ns: 1e15, delivered_at_ns: 1e15 + 2.5e9 })).toBe(2500);
    expect(deliveryWaitMs({ created_at_ns: 1e15, delivered_at_ns: null })).toBeNull();
  });
});

describe('following the stream', () => {
  const chunks = (...texts) => ({ async *[Symbol.asyncIterator]() { for (const t of texts) yield t; } });
  const failing = (status, what) => Object.assign(new Error(what), { status, what });

  it('replaces on a snapshot, upserts on a run, and reports live', async () => {
    const controller = new AbortController();
    const seen = [];
    const statuses = [];
    await runLiveStream({
      open: async () => chunks(streamText.slice(0, 40), streamText.slice(40)),
      poll: vi.fn(),
      signal: controller.signal,
      onSnapshot: (snapshot, meta) => seen.push(['snapshot', snapshot.runs.length, meta.source]),
      onRun: (row) => seen.push(['run', row.run]),
      onStatus: (status) => { statuses.push(status.state); if (status.state === 'reconnecting') controller.abort(); },
      sleep: async () => {},
    });
    expect(seen[0]).toEqual(['snapshot', listing.runs.length, 'stream']);
    expect(seen.filter((s) => s[0] === 'run')).toHaveLength(4);
    expect(statuses).toEqual(['live', 'reconnecting']);
  });

  it('hands each removal to onRemoved, in order with the run frames', async () => {
    const controller = new AbortController();
    const seen = [];
    await runLiveStream({
      open: async () => chunks(removedText),
      poll: vi.fn(),
      signal: controller.signal,
      onSnapshot: () => seen.push('snapshot'),
      onRun: (row) => seen.push('run ' + row.run),
      onRemoved: (removal) => seen.push(`removed ${removal.run} ${removal.reason}`),
      onStatus: (status) => { if (status.state === 'reconnecting') controller.abort(); },
      sleep: async () => {},
    });
    expect(seen).toEqual(['snapshot', 'removed tkt-5001 evicted', 'run tkt-5002', 'removed tkt-5002 expired']);
  });

  it('reconnects with backoff that resets on progress, starting again from a snapshot', async () => {
    const controller = new AbortController();
    const waits = [];
    let opens = 0;
    await runLiveStream({
      // Two connections that die before a byte, then one that sends a tick.
      open: async () => {
        opens += 1;
        if (opens <= 2) throw failing(0, 'The server did not respond.');
        if (opens === 3) return chunks(': tick\n\n');
        controller.abort();
        return chunks('');
      },
      poll: vi.fn(),
      signal: controller.signal,
      onSnapshot: () => {},
      onRun: () => {},
      sleep: async (ms) => { waits.push(ms); },
    });
    // 500, 1000 — then the tick is progress, and the next wait starts over.
    expect(waits).toEqual([500, 1000, 500]);
  });

  it('polls the listing when the stream is refused for want of a slot, and says why', async () => {
    const controller = new AbortController();
    let clock = 0;
    const statuses = [];
    const poll = vi.fn(async (params) => {
      expect(params).toEqual({ tenant: 'acme', limit: 256 });
      return listing;
    });
    let opens = 0;
    await runLiveStream({
      open: async () => {
        opens += 1;
        if (opens === 1) throw failing(409, 'the every-tenant view already has 8 live-run streams open; close one, or poll GET /v1/runs/live');
        controller.abort();
        return chunks('');
      },
      poll,
      params: { tenant: 'acme' },
      signal: controller.signal,
      now: () => clock,
      onSnapshot: (snapshot, meta) => expect(meta.source).toBe('poll'),
      onRun: () => {},
      onStatus: (status) => statuses.push(status.state),
      sleep: async (ms) => { clock += ms; },
    });
    expect(poll).toHaveBeenCalledTimes(STREAM_RETRY_MS / LIVE_POLL_MS);
    expect(statuses[0]).toBe('polling');
    expect(opens).toBe(2);
  });

  it('stops for good on a refused credential, a store without fuses, or a server without the route', async () => {
    for (const [error, state] of [
      [failing(401, 'This request needs a bearer token.'), 'denied'],
      [failing(409, 'live runs and nudges need run fuses, which are disabled on this store (traza-server --no-fuses)'), 'disabled'],
      // A server that predates the live view: retrying would never help.
      [failing(404, 'not found'), 'missing'],
    ]) {
      const statuses = [];
      const open = vi.fn(async () => { throw error; });
      await runLiveStream({ open, poll: vi.fn(), onSnapshot: () => {}, onRun: () => {}, onStatus: (s) => statuses.push(s.state), sleep: async () => {} });
      expect(open).toHaveBeenCalledTimes(1);
      expect(statuses).toEqual([state]);
    }
    expect(classifyFailure(failing(503, 'busy'))).toBe('broken');
  });
});


// ------------------------------------------------------------ review fixes

describe('a step asked about and not taken', () => {
  // tkt-4162 asked to run search_orders 58 s before the capture, and no span
  // has arrived since: the listing says continue for the path so far, but
  // the check's answer for the step itself is not in the listing.
  const asked = byRun('tkt-4162');
  const askedAt = asked.current.started_ns / 1e6;

  it('stops being called continue once the step has been pending a while', () => {
    expect(isWaiting(asked, askedAt + WAITING_MS - 1)).toBe(false);
    expect(isWaiting(asked, askedAt + WAITING_MS)).toBe(true);
    expect(guardState({ row: asked, waiting: true })).toBe('waiting');
    // A recorded step never waits, however old.
    expect(isWaiting(byRun('tkt-4173'), CAPTURE_MS + 3_600_000)).toBe(false);
    // A stop the verdict read found for the path so far is still held, and
    // the guard's own stop is still stop.
    expect(guardState({ row: asked, waiting: true, held: { doc: { verdict: 'stop' } } })).toBe('held');
    expect(guardState({ row: byRun('conv-82febb'), waiting: true })).toBe('stop');
  });

  it('ranks a waiting run for attention, after the guard\'s warnings and before novel paths', () => {
    const onTrack = { row: byRun('tkt-4174'), flashAt: 0 };
    expect(needsAttention(onTrack)).toBe(false);
    const waiting = { ...onTrack, waiting: true };
    expect(needsAttention(waiting)).toBe(true);
    expect(riskRank(waiting)).toBeGreaterThan(riskRank({ row: { ...byRun('tkt-4174'), verdict: 'warn' } }));
    expect(riskRank(waiting)).toBeLessThan(riskRank({ row: byRun('conv-c36f2e') }));
    // Not a stop: the stopped filter leaves it out, the attention filter keeps it.
    expect(matches(waiting, { filter: 'stopped' }, CAPTURE_MS)).toBe(false);
    expect(matches(waiting, { filter: 'attention' }, CAPTURE_MS)).toBe(true);
  });

  it('asks about the runs never asked first, the longest pending first, and each at most once per answer\'s life', () => {
    const rows = new Map(listing.runs.map((row) => [rowKey(row), { row, flashAt: 0 }]));
    const now = CAPTURE_MS + 5000;
    const opts = { afterMs: 4000, ttlMs: 10_000, batch: 4 };
    const names = (asked, nowMs = 1e6) => probeCandidates(rows, asked, now, nowMs, opts).map((c) => c.row.run);
    // The guard's stops are not asked about; tkt-4162 has waited longest.
    expect(names(new Map())).toEqual(['tkt-4162', 'tkt-4176', 'tkt-4174']);
    const key = rowKey(asked);
    expect(names(new Map([[key, { activity: asked.last_activity_ns, at: 1e6 - 2000 }]]))).toEqual(['tkt-4176', 'tkt-4174']);
    // An answer past its life is asked again — after every run never asked.
    expect(names(new Map([[key, { activity: asked.last_activity_ns, at: 1e6 - 11_000 }]]))).toEqual(['tkt-4176', 'tkt-4174', 'tkt-4162']);
    expect(probeCandidates(rows, new Map(), now, 1e6, { ...opts, batch: 2 })).toHaveLength(2);
  });
});

describe('the server\'s clock, from the listing alone', () => {
  const doc = (runs) => ({ runs, truncated: false, windowNs: 900e9 });

  it('follows a server clock behind the browser\'s down, as polling must: a run that moved between two reads moved within them', () => {
    const behind = 60_000;
    const first = CAPTURE_MS + behind;
    let state = applySnapshot(newLiveState(), doc(listing.runs), first);
    const before = byRun('tkt-4173');
    const moved = { ...before, steps: before.steps + 1, last_activity_ns: (CAPTURE_MS + 4000) * 1e6 };
    state = applySnapshot(state, doc(listing.runs.map((row) => (row.run === moved.run ? moved : row))), first + 5000);
    const server = serverNowMs(state, first + 5000);
    expect(Math.abs(server - (CAPTURE_MS + 5000))).toBeLessThan(6000);
    expect(isQuiet(moved, server)).toBe(false);
  });

  it('never ages out a run the server has just listed, however far behind its clock is', () => {
    const behind = 20 * 60_000;
    let state = applySnapshot(newLiveState(), doc(listing.runs), CAPTURE_MS + behind);
    state = ageOut(state, CAPTURE_MS + behind + 1000);
    expect(state.rows.size).toBe(listing.runs.length);
  });
});

describe('failures the stream must not retry, and ones it must say', () => {
  const chunks = (...texts) => ({ async *[Symbol.asyncIterator]() { for (const t of texts) yield t; } });
  const failing = (status, what) => Object.assign(new Error(what), { status, what });

  it('stops on a request the server refuses as malformed, and keeps its words: a 400 is not a broken connection', async () => {
    // As a 0.28.0 server answered a tenant typed into the filter.
    const error = failing(400, 'tenant must be lowercase [a-z0-9][a-z0-9._-], at most 64 bytes');
    const statuses = [];
    const open = vi.fn(async () => { throw error; });
    // Bounded, so a runner that retries forever fails here instead of spinning.
    const controller = new AbortController();
    let slept = 0;
    await runLiveStream({
      open, poll: vi.fn(), signal: controller.signal, onSnapshot: () => {}, onRun: () => {},
      onStatus: (s) => statuses.push(s), sleep: async () => { slept += 1; if (slept > 3) controller.abort(); },
    });
    expect(open).toHaveBeenCalledTimes(1);
    expect(statuses).toEqual([{ state: 'rejected', error }]);
    expect(classifyFailure(failing(400, 'x'))).toBe('rejected');
    expect(classifyFailure(failing(422, 'x'))).toBe('rejected');
    // Busy and rate-limited answers are worth retrying.
    expect(classifyFailure(failing(429, 'slow down'))).toBe('broken');
    expect(classifyFailure(failing(503, 'busy'))).toBe('broken');
  });

  it('says what failed while it reconnects', async () => {
    const controller = new AbortController();
    const statuses = [];
    let opens = 0;
    await runLiveStream({
      open: async () => {
        opens += 1;
        if (opens === 1) throw failing(503, 'the store is warming up');
        controller.abort();
        return chunks('');
      },
      poll: vi.fn(),
      signal: controller.signal,
      onSnapshot: () => {},
      onRun: () => {},
      onStatus: (s) => statuses.push(s),
      sleep: async () => {},
    });
    expect(statuses[0]).toEqual(expect.objectContaining({ state: 'reconnecting', error: expect.objectContaining({ what: 'the store is warming up' }) }));
  });

  it('takes a stream that goes silent without closing as dead — not even the heartbeat came — and reconnects', async () => {
    vi.useFakeTimers();
    try {
      const controller = new AbortController();
      const statuses = [];
      const signals = [];
      let opens = 0;
      const done = runLiveStream({
        // A snapshot, then nothing: a half-open connection after a laptop
        // slept, which no read will ever end.
        open: async (params, signal) => {
          opens += 1;
          signals.push(signal);
          if (opens === 1) return { async *[Symbol.asyncIterator]() { yield streamText; await new Promise(() => {}); } };
          controller.abort();
          return chunks('');
        },
        poll: vi.fn(),
        signal: controller.signal,
        onSnapshot: () => {},
        onRun: () => {},
        onStatus: (s) => statuses.push(s),
        sleep: async () => {},
      });
      await vi.advanceTimersByTimeAsync(STALL_MS - 100);
      expect(opens).toBe(1);
      expect(statuses.map((s) => s.state)).toEqual(['live']);
      await vi.advanceTimersByTimeAsync(200);
      await done;
      expect(opens).toBe(2);
      expect(statuses.map((s) => s.state)).toEqual(['live', 'reconnecting']);
      expect(statuses[1].error.what).toMatch(/nothing for 40 s/);
      // The dead connection is let go, not left holding a socket.
      expect(signals[0].aborted).toBe(true);
    } finally {
      vi.useRealTimers();
    }
  });

  it('counts the heartbeat as life: a tick inside the window keeps the connection', async () => {
    vi.useFakeTimers();
    try {
      const controller = new AbortController();
      let opens = 0;
      const done = runLiveStream({
        open: async () => {
          opens += 1;
          if (opens === 1) {
            return {
              async *[Symbol.asyncIterator]() {
                yield streamText;
                await new Promise((wake) => setTimeout(wake, 15_000));
                yield ': tick\n\n';
                await new Promise(() => {});
              },
            };
          }
          controller.abort();
          return chunks('');
        },
        poll: vi.fn(),
        signal: controller.signal,
        onSnapshot: () => {},
        onRun: () => {},
        sleep: async () => {},
      });
      await vi.advanceTimersByTimeAsync(STALL_MS + 5000);
      expect(opens).toBe(1);
      await vi.advanceTimersByTimeAsync(15_000);
      await done;
      expect(opens).toBe(2);
    } finally {
      vi.useRealTimers();
    }
  });
});


// ------------------------------------------------------- astra review fixes

describe('the server\'s clock, from a view that does not hold every run', () => {
  const [snapshot, nudged] = createFramer()(unseenText).map(parseLiveFrame).filter(Boolean);
  const newest = Math.max(...snapshot.runs.map((row) => row.last_activity_ns)) / 1e6;
  const asListing = (doc) => ({ runs: doc.runs, truncated: doc.truncated, windowNs: doc.window_ns });

  it('takes no reading from a run frame for a run a cut snapshot left out: its activity can be minutes old', () => {
    expect(snapshot.truncated).toBe(true);
    expect(snapshot.runs.some((row) => row.run === nudged.row.run)).toBe(false);
    // A nudge moved the row, not its activity, which is 30 s older than the rest.
    expect(nudged.row.pending_nudges).toBe(1);
    expect(newest - nudged.row.last_activity_ns / 1e6).toBeGreaterThan(29_000);
    // The browser's clock agrees with the server's.
    let state = applySnapshot(newLiveState(), snapshot, newest + 1000);
    state = applyRun(state, nudged.row, newest + 1500);
    expect(state.rows.has(rowKey(nudged.row))).toBe(true);
    expect(Math.abs(serverNowMs(state, newest + 1500) - (newest + 1500))).toBeLessThan(1000);
    // An established reading is kept, not dragged back by the old row either.
    let ahead = applyRun(newLiveState(), snapshot.runs[0], newest - 60_000);
    ahead = applySnapshot(ahead, snapshot, newest - 59_000);
    const before = serverNowMs(ahead, newest - 58_500);
    ahead = applyRun(ahead, nudged.row, newest - 58_500);
    expect(Math.abs(serverNowMs(ahead, newest - 58_500) - before)).toBeLessThan(1000);
  });

  it('takes no ceiling from a listing row it did not hold, when the listing before was cut', () => {
    // Polling: the cut snapshot, then a cut listing five seconds later that
    // leads with the nudged run.
    let state = applySnapshot(newLiveState(), snapshot, newest + 1000);
    state = applySnapshot(state, asListing(afterNudge), newest + 6000);
    expect(afterNudge.runs[0].run).toBe(nudged.row.run);
    expect(Math.abs(serverNowMs(state, newest + 6000) - (newest + 6000))).toBeLessThan(1000);
  });

  it('still reads the clock from a run that appears after a snapshot that held every run', () => {
    // An empty, whole snapshot: a run the stream then sends has just started.
    // The browser's clock runs 60 s behind the server's.
    const at = nudged.row.last_activity_ns / 1e6;
    let state = applySnapshot(newLiveState(), { runs: [], truncated: false, windowNs: 900e9 }, at - 61_000);
    state = applyRun(state, nudged.row, at - 60_000);
    expect(serverNowMs(state, at - 60_000)).toBeCloseTo(at, -1);
  });
});

describe('a view the page itself cut', () => {
  const [snapshot] = createFramer()(unseenText).map(parseLiveFrame).filter(Boolean);
  const template = snapshot.runs[0];
  const nth = (i) => ({ ...template, run: `r-${i}`, last_activity_ns: template.last_activity_ns + i * 1e6 });

  it('says its figures are partial once it has let a run go, until the server lets that run go too', () => {
    let state = applySnapshot(newLiveState(), { runs: [], truncated: false, windowNs: 900e9 }, 1000);
    for (let i = 0; i <= ROW_CAP; i += 1) state = applyRun(state, nth(i), 2000 + i);
    expect(state.rows.size).toBe(ROW_CAP);
    expect(state.truncated).toBe(false);
    // 1,001 runs live, 1,000 held: the totals leave one out, and say so.
    expect(liveTotals([...state.rows.values()], 0).live).toBe(ROW_CAP);
    expect(isPartial(state)).toBe(true);
    // The server says the run it dropped has left the view: every live run is held again.
    state = applyRemoved(state, { run: 'r-0', tenant: '', reason: 'expired' });
    expect(isPartial(state)).toBe(false);
  });

  it('is whole again after a snapshot, and partial while the server\'s own snapshot was cut', () => {
    let state = applySnapshot(newLiveState(), { runs: [], truncated: false, windowNs: 900e9 }, 1000);
    for (let i = 0; i <= ROW_CAP; i += 1) state = applyRun(state, nth(i), 2000 + i);
    expect(isPartial(state)).toBe(true);
    state = applySnapshot(state, { runs: [nth(5)], truncated: false, windowNs: 900e9 }, 9000);
    expect(isPartial(state)).toBe(false);
    expect(isPartial(applySnapshot(state, snapshot, 9500))).toBe(true);
  });

  it('takes no clock reading from a run frame for a run it let go', () => {
    let state = applySnapshot(newLiveState(), { runs: [], truncated: false, windowNs: 900e9 }, template.last_activity_ns / 1e6);
    for (let i = 0; i <= ROW_CAP; i += 1) state = applyRun(state, nth(i), template.last_activity_ns / 1e6 + i);
    const now = template.last_activity_ns / 1e6 + ROW_CAP;
    const before = serverNowMs(state, now);
    // r-0 was let go; a nudge sends it again, its activity a second old.
    state = applyRun(state, { ...nth(0), pending_nudges: 1 }, now);
    expect(Math.abs(serverNowMs(state, now) - before)).toBeLessThan(100);
  });
});

describe('what the held-run probe remembers', () => {
  it('keeps only the runs still held, and the same map when none has gone', () => {
    const rows = new Map(listing.runs.map((row) => [rowKey(row), { row, flashAt: 0 }]));
    const kept = new Map([[rowKey(byRun('tkt-4162')), { at: 1 }], [rowKey(byRun('tkt-4174')), { at: 2 }]]);
    expect(keepListed(kept, rows)).toBe(kept);
    const gone = new Map([...kept, [rowKey({ run: 'tkt-0001', tenant: '' }), { at: 3 }], [rowKey({ ...byRun('tkt-4162'), tenant: 'acme' }), { at: 4 }]]);
    expect([...keepListed(gone, rows).keys()]).toEqual([...kept.keys()]);
  });
});
