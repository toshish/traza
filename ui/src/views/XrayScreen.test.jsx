// @vitest-environment jsdom
//
// The X-Ray screen, rendered from GET /v1/cache/{run} and
// GET /v1/xray/{trace}/{span} as a 0.28.0 server answered them: the insight
// corpus's coding run (a clock in the system prompt, stale results, a pasted
// repeat, a pause past the TTL, a compaction), the imported Claude Code
// fixture (each call rebuilt from recorded deltas), and a run whose recorded
// text is markup.

import React from 'react';
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, act, cleanup, fireEvent, screen, within } from '@testing-library/react';
import autopsy from '../test/fixtures/insight/cache_fix7.json';
import xray30 from '../test/fixtures/insight/xray_fix7_30.json';
import xray34 from '../test/fixtures/insight/xray_fix7_34.json';
import ccAutopsy from '../test/fixtures/insight/cache_cc1.json';
import ccXray from '../test/fixtures/insight/xray_cc1_14.json';
import injected from '../test/fixtures/insight/xray_inject.json';

const BY_SPAN = {
  [xray30.span.span_id]: xray30,
  [xray34.span.span_id]: xray34,
  [ccXray.span.span_id]: ccXray,
  [injected.span.span_id]: injected,
};

vi.mock('../lib/api.js', () => ({
  api: {
    cache: vi.fn(async (run) => (run === 'fix-parser-7' ? autopsy : run === 'inject-1' ? {
      ...ccAutopsy, run: 'inject-1', steps: [{ ...ccAutopsy.steps[0], step_index: 2, span: injected.span }],
    } : ccAutopsy)),
    // The last call of the coding run is not in the fixtures: answer with
    // step 34's body for any span not listed.
    xray: vi.fn(async (trace, span) => BY_SPAN[span] || xray34),
    sessions: vi.fn(async () => ({ sessions: [
      { session_id: 'fix-parser-7', llm_calls: 19, tenant: '', last_end_ns: 1792306400000000000 },
      { session_id: 'no-model', llm_calls: 0, last_end_ns: 1792306400000000000 },
    ] })),
  },
}));

const { XrayScreen } = await import('./XrayScreen.jsx');
const { api } = await import('../lib/api.js');

async function show(runId, query = '') {
  const go = vi.fn();
  const view = render(<XrayScreen runId={runId} go={go} params={new URLSearchParams(query)} identity={null} pushToast={vi.fn()} />);
  await act(async () => { for (let i = 0; i < 6; i += 1) await Promise.resolve(); });
  return { view, go };
}

beforeEach(() => Object.values(api).forEach((fn) => fn.mockClear()));
afterEach(() => { cleanup(); window.history.replaceState(null, '', '#'); });

describe('the Cache Autopsy', () => {
  it('totals the breaks and what they cost, and draws one column per call', async () => {
    await show('fix-parser-7', 'step=30&tenant=');
    expect(api.cache).toHaveBeenCalledWith('fix-parser-7', { tenant: '', ttl_seconds: undefined }, expect.anything());
    expect(screen.getByTestId('tile-breaks').textContent).toContain('3');
    expect(screen.getByTestId('tile-cost').textContent).toContain('0.00578');
    const calls = screen.getAllByTestId('cache-call');
    expect(calls).toHaveLength(19);
    expect(calls.map((c) => c.getAttribute('data-cause')).filter((c) => c !== 'stable'))
      .toEqual(['new_context', 'volatile_content', 'ttl_expired', 'new_context']);
    // The clock break still had its first 154 tokens servable; what it lost
    // came after the divergence.
    const parts = (call) => Object.fromEntries([...call.querySelectorAll('[data-part]')].map((p) => [p.getAttribute('data-part'), Number(p.getAttribute('data-tokens'))]));
    expect(parts(calls[1])).toEqual({ fresh: 66, lost: 34, served: 154 });
    expect(parts(calls.find((c) => c.getAttribute('data-cause') === 'ttl_expired')).served).toBeUndefined();
  });

  it('keeps each column a button a screen reader announces, the one open pressed', async () => {
    await show('fix-parser-7', 'step=30');
    const calls = screen.getAllByTestId('cache-call');
    expect(calls.every((c) => c.tagName === 'BUTTON' && !c.hasAttribute('role'))).toBe(true);
    const timeline = screen.getByRole('group', { name: 'Model calls' });
    expect(within(timeline).getAllByRole('button')).toHaveLength(19);
    expect(within(timeline).getByRole('button', { pressed: true }).getAttribute('aria-label')).toMatch(/^step 30:/);
  });

  it('names each break shape with its diff and the fixed advice the server chose', async () => {
    await show('fix-parser-7', 'step=30');
    const culprits = screen.getAllByTestId('culprit').map((c) => c.textContent);
    expect(culprits).toHaveLength(3);
    expect(culprits[0]).toContain('ttl expired');
    expect(culprits[1]).toContain('2026-10-18T09:14:03Z');
    expect(culprits[1]).toContain('2026-10-18T09:19:44Z');
    expect(culprits[1]).toContain('A clock value sits inside the cached prefix.');
  });

  it("opens the call a column names, and the last call when none is named", async () => {
    await show('fix-parser-7', 'step=30');
    expect(api.xray).toHaveBeenCalledWith(xray30.span.trace_id, xray30.span.span_id, { ttl_seconds: undefined }, expect.anything());
    cleanup();
    api.xray.mockClear();
    await show('fix-parser-7');
    const last = autopsy.steps[autopsy.steps.length - 1].span;
    expect(api.xray).toHaveBeenCalledWith(last.trace_id, last.span_id, expect.anything(), expect.anything());
    fireEvent.click(screen.getAllByTestId('cache-call')[1]);
    expect(new URLSearchParams(window.location.hash.split('?')[1]).get('step')).toBe('2');
  });

  it('says when the step named is not a model call of the run', async () => {
    await show('fix-parser-7', 'step=1');
    expect(screen.getByTestId('missing-call').textContent).toContain("Step 1 is not one of this run's model calls");
    expect(api.xray).not.toHaveBeenCalled();
  });

  it('assumes a longer cache lifetime when told to', async () => {
    await show('fix-parser-7', 'ttl=3600');
    expect(api.cache).toHaveBeenCalledWith('fix-parser-7', { ttl_seconds: '3600' }, expect.anything());
  });

  it("counts the provider's disagreements and marks the held prefix it served nothing from", async () => {
    await show('11111111-1111-4111-8111-111111111111');
    expect(document.body.textContent).toContain('provider disagrees ×1');
    // No break: every call is listed, and step 13 is marked.
    expect(screen.getByText('all (9)')).toBeTruthy();
    const row = screen.getAllByRole('row').find((r) => r.textContent.startsWith('step 13'));
    expect(row.textContent).toContain('disagrees');
  });
});

describe('the X-Ray of one call', () => {
  it('draws the context window chunk by chunk, stale ones hatched, repeats flagged', async () => {
    await show('fix-parser-7', 'step=30');
    const segments = screen.getAllByTestId('context-segment');
    expect(segments).toHaveLength(38);
    expect(segments.filter((s) => s.getAttribute('data-stale') === 'true')).toHaveLength(2);
    expect(screen.getByTestId('tile-est').textContent).toContain('868');
    const rows = screen.getAllByTestId('chunk-row');
    expect(rows[14].textContent).toContain('stale');
    expect(rows[37].textContent).toContain('repeat of #9');
    expect(rows[7].textContent).toMatch(/step 1\s*tool Read/);
  });

  it('totals the context by the step that introduced it, and narrows the list to one step', async () => {
    await show('fix-parser-7', 'step=30');
    const chips = within(screen.getByTestId('provenance')).getAllByRole('button');
    expect(chips[0].textContent).toBe('step 7tool Read34%');
    fireEvent.click(chips[0]);
    const rows = screen.getAllByTestId('chunk-row');
    expect(rows).toHaveLength(1);
    expect(rows[0].getAttribute('data-index')).toBe('14');
  });

  it('links a chunk to the step that introduced it on the tape', async () => {
    const { go } = await show('fix-parser-7', 'step=30&tenant=');
    const rows = screen.getAllByTestId('chunk-row');
    fireEvent.click(within(rows[7]).getByText('step 1'));
    expect(go).toHaveBeenCalledWith(['run', 'fix-parser-7'], { tenant: '', step: '1' });
  });

  it('flags the instruction a compaction took away, and the re-stamped clock as an edit', async () => {
    await show('fix-parser-7', 'step=34');
    const dropped = screen.getByTestId('dropped').textContent;
    expect(dropped).toContain('1 instruction the agent was given is no longer in its context.');
    expect(dropped).toContain('Never edit files under migrations/ and never weaken a test.');
    expect(dropped).toContain('instruction lost');
    expect(screen.getByText(/more dropped chunks/)).toBeTruthy();
    expect(document.body.textContent).toContain('2026-10-18T09:31:10Z');
  });

  it('unfolds a folded dropped chunk from the keyboard', async () => {
    await show('fix-parser-7', 'step=34');
    const panel = screen.getByTestId('dropped');
    // The lost instruction opens unfolded; the rest fold to one line.
    expect(within(panel).getAllByRole('button', { expanded: true })).toHaveLength(1);
    const folded = within(panel).getAllByRole('button', { expanded: false });
    expect(folded.length).toBeGreaterThan(0);
    expect(folded[0].tagName).toBe('BUTTON');
    fireEvent.click(folded[0]);
    expect(folded[0].getAttribute('aria-expanded')).toBe('true');
  });

  it('shows a Claude Code call rebuilt from its deltas, each chunk traced to its step', async () => {
    await show('11111111-1111-4111-8111-111111111111');
    const rows = screen.getAllByTestId('chunk-row');
    expect(rows).toHaveLength(ccXray.chunks.length);
    expect(rows[4].textContent).toContain('pub struct Cart { items: Vec<Item> }');
    expect(rows[4].textContent).toMatch(/step 1\s*tool Read/);
  });
});

describe('stored text', () => {
  it('is shown as text, never as markup', async () => {
    const { view } = await show('inject-1');
    const text = view.container.textContent;
    expect(text).toContain('<script>document.title="pwned"</script> Summarise README.md');
    expect(text).toContain('<img src=x onerror=');
    expect(view.container.querySelector('script')).toBeNull();
    expect(view.container.querySelector('img')).toBeNull();
    expect(view.container.querySelector('b')).toBeNull();
  });
});

describe('no run named', () => {
  it('takes a run id, and offers recent sessions that made model calls', async () => {
    const { go } = await show(undefined);
    expect(document.body.textContent).toContain('fix-parser-7');
    expect(document.body.textContent).not.toContain('no-model');
    fireEvent.click(screen.getByText('fix-parser-7'));
    expect(go).toHaveBeenCalledWith(['xray', 'fix-parser-7'], { tenant: '' });
  });
});
