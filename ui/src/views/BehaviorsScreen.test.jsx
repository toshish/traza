// @vitest-environment jsdom
//
// The Behaviors screen, rendered from GET /v1/trajectories/search,
// /v1/trajectories/explain, /v1/detectors and /v1/detectors/findings as a
// 0.28.0 server answered them over the behaviour and insight corpora.

import React from 'react';
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, act, cleanup, fireEvent, screen, within } from '@testing-library/react';
import tamper from '../test/fixtures/insight/tql_tamper.json';
import ccSearch from '../test/fixtures/insight/tql_cc_year.json';
import explain from '../test/fixtures/insight/tql_explain.json';
import refusal from '../test/fixtures/insight/tql_bad_field.json';
import detectors from '../test/fixtures/insight/detectors.json';
import findings from '../test/fixtures/insight/findings.json';

vi.mock('../lib/api.js', () => ({
  api: {
    trajectories: vi.fn(async (params) => (params.since ? ccSearch : tamper)),
    explainQuery: vi.fn(async (q) => {
      if (q.includes('stauts')) throw Object.assign(new Error(refusal.error), { status: 400, what: refusal.error, body: refusal });
      return explain;
    }),
    detectors: vi.fn(async () => detectors),
    findings: vi.fn(async () => findings),
  },
}));

const { BehaviorsScreen } = await import('./BehaviorsScreen.jsx');
const { api } = await import('../lib/api.js');

const TAMPER = 'test[error] ..> edit[file~"(^|/)tests?/"] ..> test[not error]';

async function show(query = '') {
  const go = vi.fn();
  const view = render(<BehaviorsScreen go={go} params={new URLSearchParams(query)} identity={null} pushToast={vi.fn()} />);
  await act(async () => { for (let i = 0; i < 4; i += 1) await Promise.resolve(); });
  return { view, go };
}

beforeEach(() => Object.values(api).forEach((fn) => fn.mockClear()));
afterEach(() => { cleanup(); vi.useRealTimers(); window.history.replaceState(null, '', '#'); });

describe('a search', () => {
  it('lists the matching runs with the steps the atoms matched, each linked to its tape', async () => {
    const { go } = await show('q=' + encodeURIComponent(TAMPER));
    expect(api.trajectories).toHaveBeenCalledWith(expect.objectContaining({ q: TAMPER, limit: 50, since: undefined }), expect.anything());
    const match = screen.getByTestId('tql-match');
    expect(match.textContent).toContain('fix-parser-7');
    expect(match.textContent).toContain('success');
    expect(match.textContent).toContain('steps 5–33 of 37');
    expect(match.textContent).toContain('#5 tool:Bash');
    expect(match.textContent).toContain('#31 tool:Edit');
    fireEvent.click(within(match).getByText('fix-parser-7'));
    expect(go).toHaveBeenCalledWith(['run', 'fix-parser-7'], { step: '5', tenant: '' });
    fireEvent.click(within(match).getByTitle('step 31: tool Edit'));
    expect(go).toHaveBeenCalledWith(['run', 'fix-parser-7'], { step: '31', tenant: '' });
    expect(screen.getByTestId('tql-results').textContent).toContain('350 of 350 runs scanned');
  });

  it('reaches back a year for the imported Claude Code session, which edited after a failing test', async () => {
    await show('window=1y&q=' + encodeURIComponent('test[error] ..> edit'));
    expect(api.trajectories.mock.calls[0][0].since).toMatch(/^\d{19}$/);
    const runs = screen.getAllByTestId('tql-match').map((m) => m.textContent);
    expect(runs.some((t) => t.includes('11111111-1111-4111-8111-111111111111'))).toBe(true);
  });

  it('parses the query as it is typed, and refuses at the column the server names', async () => {
    vi.useFakeTimers();
    await show();
    fireEvent.change(screen.getByLabelText('TQL query'), { target: { value: 'tool(bash)[stauts=error]' } });
    await act(async () => { vi.advanceTimersByTime(300); });
    await act(async () => { for (let i = 0; i < 4; i += 1) await Promise.resolve(); });
    const shown = screen.getByTestId('tql-refusal').textContent;
    expect(shown).toContain('tool(bash)[stauts=error]\n' + ' '.repeat(11) + '^');
    expect(shown).toContain('unknown field `stauts`; did you mean `status`?');
    // A refused query is not searched — from the chip or the keyboard. A
    // search would put it in the route.
    const routed = () => new URLSearchParams(window.location.hash.split('?')[1]).get('q');
    fireEvent.click(screen.getAllByText('Search')[0]);
    expect(routed()).toBeNull();
    fireEvent.keyDown(screen.getByLabelText('TQL query'), { key: 'Enter', metaKey: true });
    expect(routed()).toBeNull();
    fireEvent.keyDown(screen.getByLabelText('TQL query'), { key: 'Enter', ctrlKey: true });
    expect(routed()).toBeNull();
    // Fixed, it is.
    fireEvent.change(screen.getByLabelText('TQL query'), { target: { value: 'tool(bash)[status=error]' } });
    await act(async () => { vi.advanceTimersByTime(300); });
    await act(async () => { for (let i = 0; i < 4; i += 1) await Promise.resolve(); });
    fireEvent.keyDown(screen.getByLabelText('TQL query'), { key: 'Enter', metaKey: true });
    expect(routed()).toBe('tool(bash)[status=error]');
  });

  it('says what a valid query reads as', async () => {
    vi.useFakeTimers();
    await show();
    fireEvent.change(screen.getByLabelText('TQL query'), { target: { value: 'edit[file=@f] (..> edit[file=@f]){4,} within 10' } });
    await act(async () => { vi.advanceTimersByTime(300); });
    await act(async () => { for (let i = 0; i < 4; i += 1) await Promise.resolve(); });
    const reading = screen.getByTestId('tql-explain').textContent;
    expect(reading).toContain('2 atoms');
    expect(reading).toContain('captures @f');
    expect(reading).toContain('covers 5–∞ steps');
    expect(reading).toContain('within 10 steps');
  });

  it('searches an example from the guide', async () => {
    await show();
    fireEvent.click(within(screen.getByTestId('tql-help')).getByText('edit[file=@f] ..> edit[file=@f]'));
    expect(new URLSearchParams(window.location.hash.split('?')[1]).get('q')).toBe('edit[file=@f] ..> edit[file=@f]');
  });
});

describe('the detector library', () => {
  it('ranks detectors with findings first, each against how every checked run ended', async () => {
    const { go } = await show();
    const rows = screen.getAllByTestId('detector').map((d) => d.textContent);
    expect(rows[0]).toContain('Edited the tests to make them pass');
    expect(rows[0]).toContain('1 of 350 runs');
    expect(rows).toHaveLength(findings.detectors.filter((d) => d.runs_matched).length);
    const panel = screen.getByTestId('findings').textContent;
    expect(panel).toContain('61.9%');
    expect(panel).toContain('No findings in this window:');
    expect(panel).toContain('Agents handing work back and forth');
    fireEvent.click(screen.getAllByText('giveup-5')[0]);
    expect(go).toHaveBeenCalledWith(['run', 'giveup-5'], expect.objectContaining({ tenant: '' }));
  });

  it("draws a detector's success rate with its interval, and calls it worse only when the interval says so", async () => {
    await show();
    // One run matched, and failed: 0% of 1 cannot be told from the 61.9% baseline.
    const row = screen.getAllByTestId('detector').find((d) => d.textContent.includes('hallucinated_tool'));
    expect(row.textContent).toContain('0% of 1 known');
    expect(within(row).getByRole('img', { name: /success 0\.0%, 95% interval 0\.0–79\.3%, base rate 61\.9%/ })).toBeTruthy();
    expect(within(row).getByTestId('detector-rate').style.color).not.toBe('var(--error)');
  });

  it("loads a detector's query into the search", async () => {
    await show();
    fireEvent.click(screen.getAllByText('Search this pattern')[0]);
    expect(new URLSearchParams(window.location.hash.split('?')[1]).get('q')).toBe(detectors.detectors[0].query);
  });
});
