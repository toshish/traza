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
import { createFramer } from './tail.js';
import {
  IDLE_MS, LIVE_POLL_MS, ROW_CAP, STALL_MS, STREAM_RETRY_MS, WAITING_MS, ageOut, agentsOf, applyRun, applySnapshot,
  classifyFailure, deliveryWaitMs, fmtInterval, fmtPct, foresightMarks, foresightSentence, guardState, isQuiet,
  isWaiting, liveTotals, matches, needsAttention, newLiveState, nudgeProblem, parseLiveFrame, probeCandidates,
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
