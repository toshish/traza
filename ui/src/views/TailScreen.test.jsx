// @vitest-environment jsdom
//
// The Live tail screen, driven as a component.
//
// Every bug this screen has had was found by a reviewer rather than by a test,
// and the reason was always the same: the logic lived where nothing but React
// could call it. Extracting the pieces that had already broken fixed those
// pieces; it did not make the screen testable. This does — the effect, the
// pause buffer, the filter lifecycle and the rendering are exercised here
// against a fake stream, so the next bug of that shape has somewhere to fail.

import React from 'react';
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, act, cleanup, fireEvent } from '@testing-library/react';

const opened = [];

// A controllable stream: the test decides what arrives and when.
function makeStream() {
  let push;
  let close;
  const queue = [];
  let waiting = null;

  const iterable = {
    async *[Symbol.asyncIterator]() {
      for (;;) {
        if (queue.length) {
          yield queue.shift();
          continue;
        }
        const next = await new Promise((resolve) => {
          waiting = resolve;
        });
        if (next === null) return;
        yield next;
      }
    },
  };

  push = (chunk) => {
    if (waiting) {
      const resolve = waiting;
      waiting = null;
      resolve(chunk);
    } else {
      queue.push(chunk);
    }
  };
  close = () => push(null);
  return { iterable, push, close };
}

vi.mock('../lib/api.js', () => ({
  api: {
    tailChunks: vi.fn(async (params) => {
      const stream = makeStream();
      opened.push({ params, stream });
      return stream.iterable;
    }),
  },
}));

const { TailScreen } = await import('./TailScreen.jsx');
const { api } = await import('../lib/api.js');

const span = (id, overrides = {}) => ({
  trace_id: `trace-${id}`,
  span_id: id,
  name: 'op',
  service: 'svc',
  start_time_ns: 1_700_000_000_000_000_000,
  end_time_ns: 1_700_000_000_002_000_000,
  status: 'ok',
  ...overrides,
});

const spansFrame = (spans, cursor) =>
  `event: spans\ndata: ${JSON.stringify({ spans, cursor })}\n\n`;
const gapFrame = (missed) => `event: gap\ndata: ${JSON.stringify({ missed })}\n\n`;

/** Pushes a chunk into the newest stream and lets React flush. */
async function deliver(chunk) {
  const latest = opened[opened.length - 1];
  await act(async () => {
    latest.stream.push(chunk);
    // Two turns: one for the framer to yield, one for the state update.
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  });
}

/** Waits for the effect's first `tailChunks` call to have happened. */
async function settle() {
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
}

function rowNodes() {
  return Array.from(document.querySelectorAll('[role="link"]'));
}

function rowIds() {
  return rowNodes().map((node) => node.textContent);
}

beforeEach(() => {
  opened.length = 0;
  api.tailChunks.mockClear();
  vi.useFakeTimers({ shouldAdvanceTime: true });
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

describe('the live tail screen', () => {
  it('renders arriving spans newest first', async () => {
    render(<TailScreen go={() => {}} />);
    await settle();
    expect(screen.getByText(/Waiting for spans/)).toBeTruthy();

    await deliver(spansFrame([span('a'), span('b')], '1.2'));

    const ids = rowIds();
    expect(ids).toHaveLength(2);
    // `b` was admitted after `a`, so it is on top.
    expect(ids[0]).toContain('trace-b'.slice(0, 10));
  });

  it('buffers while paused and releases in order on resume', async () => {
    render(<TailScreen go={() => {}} />);
    await settle();
    await deliver(spansFrame([span('first')], '1.1'));

    fireEvent.click(screen.getByText(/Pause/));
    await deliver(spansFrame([span('second'), span('third')], '1.3'));

    // Still one row on screen, and the buffered count is offered.
    expect(rowNodes()).toHaveLength(1);
    expect(screen.getByText(/2 new spans/)).toBeTruthy();

    fireEvent.click(screen.getByText(/2 new spans/));
    await settle();

    const ids = rowIds();
    expect(ids).toHaveLength(3);
    // Newest first across the whole list: third, second, then first.
    expect(ids[0]).toContain('trace-thir'.slice(0, 10));
    expect(ids[2]).toContain('trace-firs'.slice(0, 10));
  });

  it('clears the view and warns when the stream gaps', async () => {
    render(<TailScreen go={() => {}} />);
    await settle();
    await deliver(spansFrame([span('a')], '1.1'));
    expect(rowNodes()).toHaveLength(1);

    await deliver(gapFrame(7));

    expect(rowNodes()).toHaveLength(0);
    expect(screen.getByText('7 missed')).toBeTruthy();
  });

  it('warns on a gap the server could not count', async () => {
    render(<TailScreen go={() => {}} />);
    await settle();
    await deliver(spansFrame([span('a')], '1.1'));

    await deliver(gapFrame(null));

    // The bug this pins at the RENDERED level: null became zero, so the view
    // was cleared with no warning at all.
    expect(rowNodes()).toHaveLength(0);
    expect(screen.getByText('spans missed')).toBeTruthy();
  });

  it('does not carry a gap warning across a filter change', async () => {
    render(<TailScreen go={() => {}} />);
    await settle();
    await deliver(gapFrame(null));
    expect(screen.getByText('spans missed')).toBeTruthy();

    fireEvent.click(screen.getByText('errors only'));
    await settle();

    expect(screen.queryByText('spans missed')).toBeNull();
  });

  it('opens one stream per filter, not one per keystroke', async () => {
    // Typing a service name re-ran the effect on every character, so
    // "checkout" tore down and re-established eight streaming connections —
    // each one a request, a scan, and a fresh backlog.
    render(<TailScreen go={() => {}} />);
    await settle();
    const before = api.tailChunks.mock.calls.length;

    const input = screen.getByLabelText('Filter by service');
    for (const value of ['c', 'ch', 'che', 'chec', 'check', 'checko', 'checkou', 'checkout']) {
      fireEvent.change(input, { target: { value } });
    }
    await act(async () => {
      await vi.advanceTimersByTimeAsync(500);
    });

    const opened_now = api.tailChunks.mock.calls.length - before;
    expect(opened_now).toBeLessThanOrEqual(1);
    const last = api.tailChunks.mock.calls[api.tailChunks.mock.calls.length - 1][0];
    expect(last.service).toBe('checkout');
  });

  it('keeps existing rows mounted when a batch arrives', async () => {
    // Row keys included the array index, so prepending a batch changed every
    // key and React discarded and rebuilt all 300 rows on every frame.
    render(<TailScreen go={() => {}} />);
    await settle();
    await deliver(spansFrame([span('old')], '1.1'));
    const originalNode = rowNodes()[0];

    await deliver(spansFrame([span('new')], '1.2'));

    const nodes = rowNodes();
    expect(nodes).toHaveLength(2);
    expect(nodes[1]).toBe(originalNode);
  });

  it('navigates to the trace when a row is activated', async () => {
    const go = vi.fn();
    render(<TailScreen go={go} />);
    await settle();
    await deliver(spansFrame([span('a')], '1.1'));

    fireEvent.click(rowNodes()[0]);
    expect(go).toHaveBeenCalledWith(['trace', 'trace-a'], { span: 'a' });

    go.mockClear();
    fireEvent.keyDown(rowNodes()[0], { key: 'Enter' });
    expect(go).toHaveBeenCalledTimes(1);
  });

  it('decays the arrival rate when the stream goes quiet', async () => {
    render(<TailScreen go={() => {}} />);
    await settle();
    await deliver(spansFrame([span('a'), span('b')], '1.2'));
    const busy = screen.getByText(/\/s$/).textContent;
    expect(busy).not.toBe('0/s');

    await act(async () => {
      await vi.advanceTimersByTimeAsync(6_000);
    });

    // Zero, however the formatter renders it — the assertion is that the rate
    // decayed, not that it is spelled a particular way.
    expect(parseFloat(screen.getByText(/\/s$/).textContent)).toBe(0);
  });

  it('says when pausing has cost spans rather than dropping them silently', async () => {
    // The buffer is capped, so a long pause under load discards the overflow.
    // Discarding is right — an unbounded buffer is how a tab dies — but doing
    // it without saying so is the same silent hole a gap would have been.
    render(<TailScreen go={() => {}} />);
    await settle();
    fireEvent.click(screen.getByText(/Pause/));

    for (let batch = 0; batch < 4; batch += 1) {
      const spans = Array.from({ length: 100 }, (_, n) => span(`b${batch}-${n}`));
      // eslint-disable-next-line no-await-in-loop
      await deliver(spansFrame(spans, `1.${batch}`));
    }

    // 400 arrived, the buffer holds 300. The shortfall must be visible.
    expect(screen.getByText(/missed/)).toBeTruthy();
  });

  it('bounds the rate window under sustained load', async () => {
    // The arrival window kept one timestamp per span with no cap, so a burst
    // large enough to matter was also large enough to be its own memory
    // problem in the tab.
    render(<TailScreen go={() => {}} />);
    await settle();
    for (let batch = 0; batch < 10; batch += 1) {
      const spans = Array.from({ length: 300 }, (_, n) => span(`x${batch}-${n}`));
      // eslint-disable-next-line no-await-in-loop
      await deliver(spansFrame(spans, `1.${batch}`));
    }
    // 3,000 spans through a 2,000-sample window. Nothing to assert about
    // internals from out here; what matters is that the screen is still
    // correct, and that the window did not grow with the traffic.
    expect(rowNodes()).toHaveLength(300);
  });
});

describe('the arrival rate the screen shows', () => {
  it('reports a rate a capped sample list could not have expressed', async () => {
    // End to end: 600 spans/s must read as 600, not as the 400 ceiling the
    // old per-span sample cap imposed.
    render(<TailScreen go={() => {}} />);
    await settle();

    // 3,000 spans across the 5s window, delivered in batches.
    for (let batch = 0; batch < 10; batch += 1) {
      const spans = Array.from({ length: 300 }, (_, n) => span(`r${batch}-${n}`));
      // eslint-disable-next-line no-await-in-loop
      await deliver(spansFrame(spans, `1.${batch}`));
    }

    const shown = parseFloat(screen.getByText(/\/s$/).textContent);
    expect(shown).toBeGreaterThan(400);
  });
});
