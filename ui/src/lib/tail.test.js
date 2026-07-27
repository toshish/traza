import { describe, it, expect } from 'vitest';
import { newTailState, pollOnce, spanKey, PAGE, MAX_PAGES_PER_TICK } from './tail.js';

/** A fake `/v1/spans` that honours `since`, `limit` and `cursor` the way the
    server does — stable order, cursor as an exclusive position. */
function fakeServer(corpus) {
  const order = (a, b) => a.start_time_ns - b.start_time_ns
    || a.trace_id.localeCompare(b.trace_id)
    || a.span_id.localeCompare(b.span_id);
  const sorted = [...corpus].sort(order);
  let requests = 0;
  const fetchPage = async (params) => {
    requests += 1;
    let rows = sorted.filter((s) => params.since == null || s.start_time_ns >= params.since);
    if (params.cursor) {
      const at = rows.findIndex((s) => spanKey(s) === params.cursor);
      rows = at === -1 ? [] : rows.slice(at + 1);
    }
    const limit = params.limit ?? 100;
    const page = rows.slice(0, limit);
    const more = rows.length > limit;
    return {
      spans: page,
      next_cursor: more && page.length ? spanKey(page[page.length - 1]) : null,
    };
  };
  return { fetchPage, requests: () => requests, add: (s) => sorted.push(s) && sorted.sort(order) };
}

const span = (index, startNs) => ({
  trace_id: `t-${String(index).padStart(5, '0')}`,
  span_id: 's1',
  start_time_ns: startNs,
  end_time_ns: startNs + 1_000_000,
  service: 'svc',
  name: 'op',
  status: 'ok',
});

describe('the live tail drains an equal-timestamp burst completely', () => {
  it('reaches every span of a burst larger than one tick can page', async () => {
    // The exact case the review reproduced: 1,250 spans at ONE timestamp.
    // A watermark cannot separate them — all are `>= since` — so only a
    // carried cursor chain can finish the burst. A per-tick budget that threw
    // the cursor away stranded rows 1,000-1,249 forever.
    const corpus = Array.from({ length: 1250 }, (_, i) => span(i, 5_000));
    const server = fakeServer(corpus);
    const state = newTailState();
    state.sinceNs = 0; // watching from before the burst

    // Poll a fixed number of times, deliberately CONTINUING past the drain.
    // The previous version of this test stopped the moment the corpus was
    // complete, which is precisely where the replay started: the next poll
    // returned the first 1,000 all over again.
    const collected = [];
    for (let tick = 0; tick < 12; tick += 1) {
      // eslint-disable-next-line no-await-in-loop
      collected.push(...await pollOnce(state, server.fetchPage, { now: 1_000_000 }));
    }

    expect(collected.length).toBe(1250);
    expect(new Set(collected.map(spanKey)).size).toBe(1250);
    // And nothing beyond row 999 was missed — the specific rows the review
    // showed were never requested.
    for (const index of [0, 199, 200, 999, 1000, 1249]) {
      expect(collected.some((s) => s.trace_id === span(index, 0).trace_id)).toBe(true);
    }
  });

  it('carries the unfinished chain rather than discarding it', async () => {
    const corpus = Array.from({ length: PAGE * MAX_PAGES_PER_TICK + 50 }, (_, i) => span(i, 7_000));
    const server = fakeServer(corpus);
    const state = newTailState();
    state.sinceNs = 0;

    const first = await pollOnce(state, server.fetchPage, { now: 1_000_000 });
    expect(first.length).toBe(PAGE * MAX_PAGES_PER_TICK);
    expect(state.chain).not.toBeNull();
    // The watermark must NOT have moved while the chain is open: moving it is
    // what made the next tick re-read the burst's prefix.
    expect(state.sinceNs).toBe(0);

    const second = await pollOnce(state, server.fetchPage, { now: 1_000_000 });
    expect(second.length).toBe(50);
    expect(state.chain).toBeNull();
    expect(state.sinceNs).toBe(7_000);
  });

  it('does not re-deliver spans once the watermark settles', async () => {
    // `since` is inclusive, so the boundary span comes back. It must be
    // dropped, on the paused path as well as the live one — the paused buffer
    // used to accumulate the same quiet page until it hit its cap.
    const corpus = [span(1, 1_000), span(2, 2_000), span(3, 3_000)];
    const server = fakeServer(corpus);
    const state = newTailState();
    state.sinceNs = 0;

    const first = await pollOnce(state, server.fetchPage, { now: 1_000_000 });
    expect(first.length).toBe(3);
    expect(state.sinceNs).toBe(3_000);

    for (let quiet = 0; quiet < 5; quiet += 1) {
      // eslint-disable-next-line no-await-in-loop
      const again = await pollOnce(state, server.fetchPage, { now: 1_000_000 });
      expect(again).toEqual([]);
    }
  });

  it('picks up spans that arrive after the burst', async () => {
    const corpus = Array.from({ length: 300 }, (_, i) => span(i, 4_000));
    const server = fakeServer(corpus);
    const state = newTailState();
    state.sinceNs = 0;

    let total = (await pollOnce(state, server.fetchPage, { now: 1_000_000 })).length;
    while (state.chain) {
      // eslint-disable-next-line no-await-in-loop
      total += (await pollOnce(state, server.fetchPage, { now: 1_000_000 })).length;
    }
    expect(total).toBe(300);

    server.add(span(9001, 9_000));
    const later = await pollOnce(state, server.fetchPage, { now: 1_000_000 });
    expect(later.map((s) => s.start_time_ns)).toEqual([9_000]);
  });

  it('never re-delivers a burst it has already drained', async () => {
    // The failure mode this pins: 1,250 spans at one timestamp cannot advance
    // the watermark, so evicting their keys by SIZE made the poll return
    // 1000, 250, 1000, 250… indefinitely.
    const corpus = Array.from({ length: 1250 }, (_, i) => span(i, 5_000));
    const server = fakeServer(corpus);
    const state = newTailState();
    state.sinceNs = 0;

    let drained = 0;
    for (let tick = 0; tick < 4; tick += 1) {
      // eslint-disable-next-line no-await-in-loop
      drained += (await pollOnce(state, server.fetchPage, { now: 1_000_000 })).length;
    }
    expect(drained).toBe(1250);

    // Every later poll must be silent.
    for (let tick = 0; tick < 6; tick += 1) {
      // eslint-disable-next-line no-await-in-loop
      expect(await pollOnce(state, server.fetchPage, { now: 1_000_000 })).toEqual([]);
    }
  });

  it('retains exactly the keys on the watermark, and prunes when it moves', async () => {
    // The set must hold one timestamp's membership: enough to dedupe an
    // inclusive floor, and no more. Pruning against only the current tick's
    // spans — the first attempt — forgot the earlier ticks' keys, which are
    // equally on the watermark, and replayed exactly the prefix they covered.
    const corpus = Array.from({ length: 600 }, (_, i) => span(i, 5_000));
    const server = fakeServer(corpus);
    const state = newTailState();
    state.sinceNs = 0;

    while (true) {
      // eslint-disable-next-line no-await-in-loop
      const batch = await pollOnce(state, server.fetchPage, { now: 1_000_000 });
      if (!state.chain && batch.length === 0) break;
    }
    expect(state.sinceNs).toBe(5_000);
    expect(state.seen.size).toBe(600); // all of them sit on the watermark

    // A span at a LATER timestamp moves the watermark and the set shrinks to
    // that timestamp's membership.
    server.add(span(9001, 9_000));
    let arrived = [];
    for (let tick = 0; tick < 4 && !arrived.length; tick += 1) {
      // eslint-disable-next-line no-await-in-loop
      arrived = await pollOnce(state, server.fetchPage, { now: 1_000_000 });
    }
    expect(arrived.length).toBe(1);
    expect(state.sinceNs).toBe(9_000);
    expect(state.seen.size).toBe(1);
  });

  it('bounds the dedupe set instead of growing without limit', async () => {
    const state = newTailState();
    const corpus = Array.from({ length: 2_000 }, (_, i) => span(i, 1_000 + i));
    const server = fakeServer(corpus);
    state.sinceNs = 0;
    for (let tick = 0; tick < 20; tick += 1) {
      // eslint-disable-next-line no-await-in-loop
      await pollOnce(state, server.fetchPage, { now: 1_000_000 });
    }
    // Distinct timestamps, so only the last one's membership is retained.
    expect(state.seen.size).toBeLessThanOrEqual(PAGE * MAX_PAGES_PER_TICK);
  });
});
