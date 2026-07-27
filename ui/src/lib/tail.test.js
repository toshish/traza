import { describe, it, expect } from 'vitest';
import {
  newGapState,
  recordGap,
  recordDropped,
  gapLabel,
  gapDetail,
  newRateWindow,
  recordArrivals,
  ratePerSecond,
  RATE_BUCKET_MS,
  RATE_WINDOW_MS,
  newTailState, runTail, createFramer, parseFrame, backoffMs,
  RECONNECT_MIN_MS, RECONNECT_MAX_MS,
} from './tail.js';

/** An SSE frame, as the server writes it. */
const spansFrame = (spans, cursor) =>
  `event: spans\ndata: ${JSON.stringify({ spans, cursor })}\n\n`;
const gapFrame = (missed) => `event: gap\ndata: ${JSON.stringify({ missed })}\n\n`;

const span = (id, startNs) => ({
  trace_id: `t-${id}`, span_id: id, start_time_ns: startNs,
  end_time_ns: startNs + 1_000, service: 'svc', name: 'op', status: 'ok',
});

/** Turns a list of text pieces into the async iterable `open` must resolve. */
function source(pieces) {
  return {
    async *[Symbol.asyncIterator]() {
      for (const piece of pieces) yield piece;
    },
  };
}

describe('the framer', () => {
  it('carries a frame split across chunk boundaries', () => {
    // TCP puts boundaries wherever it likes, routinely mid-frame. Parsing each
    // chunk independently drops every frame that straddles one.
    const framer = createFramer();
    expect(framer('event: spa')).toEqual([]);
    expect(framer('ns\ndata: {"spans":[],"cursor":"1.2"}')).toEqual([]);
    const frames = framer('\n\n');
    expect(frames).toHaveLength(1);
    expect(parseFrame(frames[0])).toEqual({ type: 'spans', spans: [], cursor: '1.2' });
  });

  it('yields every complete frame in one chunk', () => {
    const framer = createFramer();
    const frames = framer(spansFrame([span('a', 1)], '1.1') + spansFrame([span('b', 2)], '1.2'));
    expect(frames).toHaveLength(2);
    expect(parseFrame(frames[1]).cursor).toBe('1.2');
  });

  it('treats a comment as no payload rather than as an error', () => {
    // The heartbeat. A client that choked on it would drop the connection
    // every fifteen seconds on a quiet store.
    expect(parseFrame(': tick')).toBeNull();
  });

  it('ignores an unknown event instead of failing', () => {
    // Forward compatibility: a frame type added later must not break a client
    // that predates it.
    expect(parseFrame('event: weather\ndata: {"sky":"grey"}')).toBeNull();
    expect(parseFrame('event: spans\ndata: not json')).toBeNull();
  });
});

describe('backoff', () => {
  it('grows and then stops growing', () => {
    expect(backoffMs(0)).toBe(RECONNECT_MIN_MS);
    expect(backoffMs(1)).toBe(RECONNECT_MIN_MS * 2);
    expect(backoffMs(99)).toBe(RECONNECT_MAX_MS);
  });
});

describe('the tail client', () => {
  it('delivers a span that started before one already seen', async () => {
    // The bug the whole redesign exists for. Under `?since=<watermark>` paging,
    // `b` — which started at 5,000, before `a` — could never be delivered
    // after `a` had moved the watermark to 10,000. In admission order it is
    // simply the next position.
    const state = newTailState();
    const delivered = [];
    const controller = new AbortController();

    await runTail(state, {
      open: async () => source([
        spansFrame([span('a', 10_000)], '1.1'),
        spansFrame([span('b', 5_000)], '1.2'),
      ]),
      signal: controller.signal,
      sleep: async () => controller.abort(),
      onSpans: (spans) => delivered.push(...spans.map((s) => s.span_id)),
    });

    expect(delivered).toEqual(['a', 'b']);
    expect(state.cursor).toBe('1.2');
  });

  it('resumes from its position rather than replaying history', async () => {
    const state = newTailState();
    const asked = [];
    const controller = new AbortController();
    let opened = 0;

    await runTail(state, {
      open: async (params) => {
        asked.push(params);
        opened += 1;
        if (opened === 1) return source([spansFrame([span('a', 1)], '1.5')]);
        controller.abort();
        return source([]);
      },
      backfill: 200,
      signal: controller.signal,
      sleep: async () => {},
      onSpans: () => {},
    });

    // First connection asks for a backlog; the reconnect asks for a position
    // and NO backlog, or every blip would re-render the whole screen.
    expect(asked[0]).toEqual({ backfill: 200 });
    expect(asked[1]).toEqual({ cursor: '1.5' });
  });

  it('carries the filter onto every connection', async () => {
    const state = newTailState();
    const asked = [];
    const controller = new AbortController();

    await runTail(state, {
      open: async (params) => {
        asked.push(params);
        controller.abort();
        return source([]);
      },
      filter: { service: 'api', status: 'error' },
      signal: controller.signal,
      sleep: async () => {},
    });

    expect(asked[0]).toMatchObject({ service: 'api', status: 'error' });
  });

  it('backs off a server that accepts and immediately closes', async () => {
    // The reconnect loop must escalate here. Resetting the backoff on a
    // successful `open` — the first implementation — meant a server that
    // accepted the connection and then dropped it counted as a success every
    // time, and the client reconnected every 500ms forever.
    const state = newTailState();
    const controller = new AbortController();
    let opened = 0;
    const waits = [];

    await runTail(state, {
      open: async () => {
        opened += 1;
        if (opened >= 3) controller.abort();
        return source([]);
      },
      signal: controller.signal,
      sleep: async (ms) => { waits.push(ms); },
    });

    expect(opened).toBe(3);
    expect(waits[0]).toBe(RECONNECT_MIN_MS);
    expect(waits[1]).toBe(RECONNECT_MIN_MS * 2);
  });

  it('resets the backoff on progress, including a bare heartbeat', async () => {
    // A quiet store sends only heartbeats, and a tail watching one all night
    // must still come back quickly from a blip — not at whatever delay the
    // last outage escalated to.
    const state = newTailState();
    const controller = new AbortController();
    const waits = [];
    let opened = 0;

    await runTail(state, {
      open: async () => {
        opened += 1;
        if (opened === 1) throw new Error('refused');
        if (opened === 2) return source([': tick\n\n']);
        controller.abort();
        return source([]);
      },
      signal: controller.signal,
      sleep: async (ms) => { waits.push(ms); },
      onSpans: () => {},
    });

    expect(waits[0]).toBe(RECONNECT_MIN_MS);
    expect(waits[1]).toBe(RECONNECT_MIN_MS);
  });

  it('drops its position on a gap and rebuilds from what follows', async () => {
    // A gap is a discontinuity, not a position. The old contract handed back
    // the ring's floor and told the client to "backfill only what was dropped"
    // from an event-time query that cannot address an admission range — the
    // fetch overlapped the entries the stream then replayed, and the same span
    // appeared twice with nothing to deduplicate it.
    const state = newTailState();
    const gaps = [];
    const delivered = [];
    const controller = new AbortController();

    await runTail(state, {
      open: async () => source([
        spansFrame([span('before', 1)], '1.7'),
        gapFrame(412),
        spansFrame([span('after', 9)], '1.900'),
      ]),
      signal: controller.signal,
      sleep: async () => controller.abort(),
      onGap: async (missed) => { gaps.push(missed); },
      onSpans: (spans) => delivered.push(...spans.map((s) => s.span_id)),
    });

    // The count is reported so the break is visible rather than silent.
    expect(gaps).toEqual([412]);
    expect(delivered).toEqual(['before', 'after']);
    // And the position that followed the gap is the one now held — never the
    // dead one from before it.
    expect(state.cursor).toBe('1.900');
  });

  it('reconnects from scratch after a gap, not from the dead position', async () => {
    // If the connection drops during recovery, resuming from the pre-gap
    // position would gap again immediately, forever.
    const state = newTailState();
    const asked = [];
    const controller = new AbortController();
    let opened = 0;

    await runTail(state, {
      open: async (params) => {
        asked.push(params);
        opened += 1;
        if (opened === 1) return source([spansFrame([span('a', 1)], '1.5'), gapFrame(9)]);
        controller.abort();
        return source([]);
      },
      backfill: 200,
      signal: controller.signal,
      sleep: async () => {},
      onSpans: () => {},
      onGap: () => {},
    });

    expect(asked[0]).toEqual({ backfill: 200 });
    expect(asked[1]).toEqual({ backfill: 200 });
    expect(asked[1].cursor).toBeUndefined();
  });

  it('does not advance past spans whose delivery threw', async () => {
    // Advancing first would lose them invisibly. Re-sending them after a
    // reconnect is a visible duplicate, which is the better failure.
    const state = newTailState();
    const controller = new AbortController();
    let opened = 0;

    await runTail(state, {
      open: async () => {
        opened += 1;
        if (opened > 1) {
          controller.abort();
          return source([]);
        }
        return source([spansFrame([span('a', 1)], '1.9')]);
      },
      signal: controller.signal,
      sleep: async () => {},
      onSpans: () => { throw new Error('render failed'); },
    });

    expect(state.cursor).toBeNull();
  });

  it('stops immediately when aborted', async () => {
    const state = newTailState();
    const controller = new AbortController();
    controller.abort();
    let opened = 0;

    await runTail(state, {
      open: async () => { opened += 1; return source([]); },
      signal: controller.signal,
      sleep: async () => {},
    });

    expect(opened).toBe(0);
  });

  it('announces reconnecting so the screen can stop claiming to be live', async () => {
    const state = newTailState();
    const controller = new AbortController();
    const announced = [];
    let opened = 0;

    await runTail(state, {
      open: async () => {
        opened += 1;
        if (opened >= 2) controller.abort();
        return source([]);
      },
      signal: controller.signal,
      sleep: async () => {},
      onStatus: (status) => announced.push(status),
    });

    expect(announced).toContain('live');
    expect(announced).toContain('reconnecting');
  });
});

describe('a gap the server cannot count is still a visible gap', () => {
  it('reports an unknown count as null, never as zero', async () => {
    // A cursor from before a restart cannot be compared to the new process's
    // numbering, so `missed` is genuinely unknown and arrives as null. Folding
    // that into a running total made it zero, which rendered no warning while
    // the view was cleared underneath — the invisible discontinuity this
    // feature exists to remove, reintroduced at the last hop. The consumer has
    // to be able to tell "none were lost" from "we cannot know how many".
    const state = newTailState();
    state.cursor = '111.5';
    const seen = [];
    const controller = new AbortController();

    await runTail(state, {
      open: async () => source([gapFrame(null)]),
      signal: controller.signal,
      sleep: async () => controller.abort(),
      onGap: (missed) => seen.push(missed),
    });

    expect(seen).toEqual([null]);
    expect(seen[0]).not.toBe(0);
    expect(state.cursor).toBeNull();
  });

  it('passes a known count through as a number', async () => {
    const state = newTailState();
    state.cursor = '111.5';
    const seen = [];
    const controller = new AbortController();

    await runTail(state, {
      open: async () => source([gapFrame(42)]),
      signal: controller.signal,
      sleep: async () => controller.abort(),
      onGap: (missed) => seen.push(missed),
    });

    expect(seen).toEqual([42]);
  });
});

describe('gap bookkeeping', () => {
  it('keeps an uncounted break visible behind a counted one', () => {
    // The reported bug: `onGap(null)` then `onGap(5)` rendered exactly
    // "5 missed", hiding the earlier loss and claiming a precision the data
    // does not have.
    let state = newGapState();
    state = recordGap(state, null);
    expect(gapLabel(state)).toBe('spans missed');
    state = recordGap(state, 5);
    expect(gapLabel(state)).toBe('5+ missed');
  });

  it('reports an exact figure only when every break was counted', () => {
    let state = newGapState();
    state = recordGap(state, 3);
    state = recordGap(state, 2);
    expect(gapLabel(state)).toBe('5 missed');
  });

  it('shows nothing until something is actually lost', () => {
    expect(gapLabel(newGapState())).toBeNull();
    // A counted gap of zero is a real answer — the server knew, and it was
    // none — so it must not raise a warning either.
    expect(gapLabel(recordGap(newGapState(), 0))).toBeNull();
  });

  it('starts clean, so a rebuilt subscription inherits nothing', () => {
    // Resetting only the counted half carried the uncounted flag across a
    // filter change, and the new subscription showed a warning earned by the
    // old one.
    let state = newGapState();
    state = recordGap(state, null);
    state = recordGap(state, 9);
    expect(gapLabel(state)).toBe('9+ missed');
    expect(gapLabel(newGapState())).toBeNull();
  });
});

describe('spans dropped by a full pause buffer', () => {
  it('counts toward the same warning as a server-side gap', () => {
    // Different cause, same consequence for the reader: spans that existed and
    // will not appear here. Dropping the overflow is correct — an unbounded
    // pause buffer is how a tab dies — but doing it silently is not.
    let state = newGapState();
    state = recordDropped(state, 100);
    expect(gapLabel(state)).toBe('100 missed');
    state = recordGap(state, 5);
    expect(gapLabel(state)).toBe('105 missed');
  });

  it('ignores a drop of nothing', () => {
    expect(gapLabel(recordDropped(newGapState(), 0))).toBeNull();
  });

  it('explains each cause separately, because one number cannot', () => {
    let state = newGapState();
    state = recordGap(state, 5);
    state = recordDropped(state, 100);
    state = recordGap(state, null);
    const detail = gapDetail(state);
    expect(detail).toContain('5 never reached this tab');
    expect(detail).toContain('100 arrived while paused');
    expect(detail).toContain('could not be counted');
    // And the label is a floor, because one of the three breaks has no number.
    expect(gapLabel(state)).toBe('105+ missed');
  });

  it('says nothing when nothing is missing', () => {
    expect(gapDetail(newGapState())).toBe('');
  });
});

describe('the arrival-rate window', () => {
  it('reports a rate above what a capped sample list could express', () => {
    // The bug: the window kept one timestamp per span, capped at 2,000, and
    // divided by five seconds — so it could not report more than 400/s
    // whatever arrived. A 600/s stream read 400/s and looked stable.
    let window = newRateWindow();
    const start = 1_000_000;
    // 3,000 spans over 5 seconds = 600/s.
    for (let tick = 0; tick < 20; tick += 1) {
      window = recordArrivals(window, 150, start + tick * RATE_BUCKET_MS);
    }
    const observed = ratePerSecond(window, start + RATE_WINDOW_MS - 1);
    expect(observed).toBe(600);
    expect(observed).toBeGreaterThan(400);
  });

  it('stays bounded however many spans arrive', () => {
    let window = newRateWindow();
    const start = 1_000_000;
    for (let tick = 0; tick < 400; tick += 1) {
      window = recordArrivals(window, 10_000, start + tick * RATE_BUCKET_MS);
    }
    // One bucket per 250ms of a 5s window, whatever the traffic.
    expect(window.buckets.length).toBeLessThanOrEqual(RATE_WINDOW_MS / RATE_BUCKET_MS);
  });

  it('decays to zero once the window empties', () => {
    let window = newRateWindow();
    const start = 1_000_000;
    window = recordArrivals(window, 500, start);
    expect(ratePerSecond(window, start)).toBe(100);
    expect(ratePerSecond(window, start + RATE_WINDOW_MS + 1)).toBe(0);
  });

  it('merges arrivals landing in the same bucket', () => {
    let window = newRateWindow();
    const start = 1_000_000;
    window = recordArrivals(window, 5, start);
    window = recordArrivals(window, 5, start + 10);
    window = recordArrivals(window, 5, start + 20);
    expect(window.buckets).toHaveLength(1);
    expect(window.buckets[0].count).toBe(15);
  });
});
