// @vitest-environment jsdom
//
// The Guard screen, rendered from GET /v1/fuses/runs and GET
// /v1/fuses/policies exactly as a 0.27.0 server answered them (fixtures),
// with the kill switch and the reset driven through their dialogs.
//
// The fake listing filters by `state` the way the server does (stop and warn
// are verdict classes; no state is every run), so a test can tell a screen
// that asks the server for its stopped runs from one that filters a page.

import React from 'react';
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, act, cleanup, fireEvent, screen, within } from '@testing-library/react';
import fuseRuns from '../test/fixtures/fuse_runs.json';
import policies from '../test/fixtures/policies.json';
// A policy with a predictive limit and both its companions, as a 0.28.0
// server stored and listed it.
import predictive from '../test/fixtures/policies_predictive.json';

/** The server's listing: `state` selects a verdict class, `limit` cuts the
    most recently active first. */
function listing(runs, params = {}) {
  const picked = params.state ? runs.filter((run) => run.verdict === params.state) : runs;
  return { runs: params.limit ? picked.slice(0, params.limit) : picked };
}

vi.mock('../lib/api.js', () => ({
  api: {
    fuseRuns: vi.fn(async (params) => listing(fuseRuns.runs, params)),
    policies: vi.fn(async () => policies),
    fuseTrip: vi.fn(async (run) => ({ ...fuseRuns.runs[0], run, verdict: 'stop', tripped_by: 'manual' })),
    fuseReset: vi.fn(async (run) => ({ ...fuseRuns.runs[0], run, verdict: 'continue' })),
    savePolicy: vi.fn(async (body) => ({ ...body, id: 'fp_1', tenant: '' })),
    deletePolicy: vi.fn(async () => ({ deleted: true })),
  },
}));

const { GuardScreen, guardLists, parseLimits } = await import('./GuardScreen.jsx');
const { api } = await import('../lib/api.js');

const RW = { auth: 'token', scope: 'rw', operator: true, tenant: null };
const RO = { auth: 'token', scope: 'ro', operator: false, tenant: 'acme' };

async function show(identity, params = new URLSearchParams()) {
  const view = render(<GuardScreen go={vi.fn()} params={params} identity={identity} pushToast={vi.fn()} />);
  await act(async () => { for (let i = 0; i < 4; i += 1) await Promise.resolve(); });
  return view;
}

function card(run) {
  return screen.getAllByTestId('guard-run').find((node) => node.textContent.startsWith(run));
}

beforeEach(() => {
  Object.values(api).forEach((fn) => fn.mockClear());
  api.fuseRuns.mockImplementation(async (params) => listing(fuseRuns.runs, params));
  Object.defineProperty(document, 'hidden', { configurable: true, get: () => false });
});

afterEach(cleanup);

describe('the run list', () => {
  it('shows every run with its verdict, reasons and usage against its limits', async () => {
    await show(RW);
    const stopped = card('sess-7f3a');
    expect(stopped.dataset.verdict).toBe('stop');
    expect(stopped.textContent).toContain('max_repeats 4 reached: tool:search_orders#be2c24774ede8979 repeated 4 times');
    expect(within(stopped).getByTestId('usage-max_repeats').textContent).toContain('4 / 4');
    expect(within(stopped).getByTestId('usage-max_cost_usd').textContent).toContain('$0.420 / $2.00');
    expect(within(stopped).getByTestId('usage-max_steps').textContent).toContain('6 / 40');
    // The repeated step and the span that tripped the stop, as evidence.
    expect(stopped.textContent).toContain('×4');
    expect(within(stopped).getByText(/evidence/).getAttribute('href'))
      .toBe('#/trace/7e3c2d1a9f4b6c5d0e8f3a2b1c0d9e8f?span=r4');

    expect(card('sess-9c21').dataset.verdict).toBe('warn');
    expect(card('sess-9c21').textContent).toContain('max_cost_usd 2 at 85%: $1.7 spent');
  });

  it('opens a default-tenant run in the default tenant, named explicitly', async () => {
    const go = vi.fn();
    render(<GuardScreen go={go} params={new URLSearchParams()} identity={RW} pushToast={vi.fn()} />);
    await act(async () => { for (let i = 0; i < 4; i += 1) await Promise.resolve(); });
    fireEvent.click(within(card('sess-7f3a')).getByText('Tape'));
    // `tenant=` (empty) is the default tenant on the tape route; no
    // parameter would read every tenant's run of that id.
    expect(go).toHaveBeenCalledWith(['run', 'sess-7f3a'], { tenant: '' });
    fireEvent.click(within(card('acme-1')).getByText('Tape'));
    expect(go).toHaveBeenCalledWith(['run', 'acme-1'], { tenant: 'acme' });
  });

  it('asks the server for stopped and warning runs, so a quiet stopped run past the active page still shows', async () => {
    // 250 busy runs fill the most-recently-active page; sess-quiet stopped
    // ten minutes ago and has taken no step since, so it sorts below them.
    const busy = Array.from({ length: 250 }, (_, i) => ({ ...fuseRuns.runs[0], run: 'busy-' + i, tenant: '', verdict: 'continue', reasons: [] }));
    const quiet = { ...fuseRuns.runs.find((run) => run.verdict === 'stop'), run: 'sess-quiet' };
    api.fuseRuns.mockImplementation(async (params) => listing([...busy, quiet], params));
    const view = await show(RW);

    expect(api.fuseRuns).toHaveBeenCalledWith(expect.objectContaining({ state: 'stop' }), expect.anything());
    expect(api.fuseRuns).toHaveBeenCalledWith(expect.objectContaining({ state: 'warn' }), expect.anything());
    expect(view.container.textContent).toMatch(/stopped\s*1(?!\d)/);
    // Stopped first in the "all" view, even though the active page lacks it.
    expect(screen.getAllByTestId('guard-run')[0].textContent).toMatch(/^sess-quiet/);
    // The active page was full: its count says there are more.
    expect(view.container.textContent).toMatch(/all\s*201\+/);

    cleanup();
    await show(RW, new URLSearchParams('state=stop'));
    expect(screen.getAllByTestId('guard-run').map((n) => n.textContent.split('stop')[0])).toEqual(['sess-quiet']);
  });

  it('counts runs by verdict and filters by the route', async () => {
    const view = await show(RW);
    const chips = view.container.textContent;
    expect(chips).toMatch(/stopped\s*1/);
    expect(chips).toMatch(/warning\s*1/);
    cleanup();
    await show(RW, new URLSearchParams('state=stop'));
    expect(screen.getAllByTestId('guard-run').map((n) => n.dataset.verdict)).toEqual(['stop']);
  });

  it('shows the global default and the stored policies', async () => {
    const view = await show(RW);
    expect(view.container.textContent).toContain('max_repeats 12');
    const row = screen.getByTestId('policy-row');
    expect(row.textContent).toContain('support');
    expect(row.textContent).toContain('service support-agent');
    expect(row.textContent).toContain('stop');
  });
});

describe('the brake', () => {
  it('offers the kill switch and approval to a writer, and neither to a read-only token', async () => {
    await show(RW);
    expect(within(card('sess-7f3a')).getByText('Approve to continue')).toBeTruthy();
    expect(within(card('sess-9c21')).getByText('Kill switch')).toBeTruthy();
    cleanup();
    const view = await show(RO);
    expect(view.container.textContent).not.toContain('Kill switch');
    expect(view.container.textContent).not.toContain('Approve to continue');
    expect(view.container.textContent).toMatch(/need\s*rw/);
  });

  it("trips a run in its own tenant, with the reason given", async () => {
    await show(RW);
    fireEvent.click(within(card('acme-1')).getByText('Kill switch'));
    const dialog = screen.getByRole('dialog', { name: 'Kill switch' });
    fireEvent.change(within(dialog).getByRole('textbox'), { target: { value: 'looping on search' } });
    await act(async () => { fireEvent.click(within(dialog).getByText('Stop this run')); });
    expect(api.fuseTrip).toHaveBeenCalledWith('acme-1', 'looping on search', { tenant: 'acme' });
  });

  it('resets a stopped run with a grant of the limits typed', async () => {
    await show(RW);
    fireEvent.click(within(card('sess-7f3a')).getByText('Approve to continue'));
    const dialog = screen.getByRole('dialog', { name: 'Approve to continue' });
    fireEvent.change(within(dialog).getByLabelText('max_repeats'), { target: { value: '8' } });
    await act(async () => { fireEvent.click(within(dialog).getByText('Reset the run')); });
    // The run's own tenant, the default one named explicitly.
    expect(api.fuseReset).toHaveBeenCalledWith('sess-7f3a', { max_repeats: 8 }, { tenant: '' });
  });
});

describe('guardLists', () => {
  const run = (name, verdict, tenant = '') => ({ run: name, tenant, verdict });

  it('lists each run once, stopped then warning then the active page, and marks a full page', () => {
    // sess-a stopped between the reads: the active page still says warn.
    const lists = guardLists(
      { runs: [run('sess-a', 'stop')], more: false },
      { runs: [run('sess-b', 'warn')], more: false },
      { runs: [run('sess-c', 'continue'), run('sess-a', 'warn'), run('sess-a', 'continue', 'acme')], more: true },
    );
    expect(lists.all.runs.map((r) => `${r.tenant}/${r.run}`)).toEqual(['/sess-a', '/sess-b', '/sess-c', 'acme/sess-a']);
    expect(lists.all.more).toBe(true);
    expect(lists.stop).toEqual({ runs: [run('sess-a', 'stop')], more: false });
    // Continuing has no server filter: it is the active page's, and its
    // count is marked when that page was full.
    expect(lists.continue.runs.map((r) => r.run)).toEqual(['sess-c', 'sess-a']);
    expect(lists.continue.more).toBe(true);
  });
});

describe('parseLimits', () => {
  it('drops blanks, keeps numbers, and refuses what the server would', () => {
    expect(parseLimits({ max_cost_usd: '2.5', max_steps: '40', max_tokens: '' }))
      .toEqual({ limits: { max_cost_usd: 2.5, max_steps: 40 } });
    expect(parseLimits({ max_steps: '0' }).error).toMatch(/positive/);
    expect(parseLimits({ max_steps: '2.5' }).error).toMatch(/whole/);
  });
});

describe('a predictive policy', () => {
  it('saves every limit it was loaded with: editing one field never drops the predictive companions', async () => {
    api.policies.mockImplementation(async () => predictive);
    await show(RW);
    fireEvent.click(within(screen.getByTestId('policy-row')).getByText('Edit'));
    const dialog = screen.getByRole('dialog', { name: 'Edit policy' });
    expect(within(dialog).getByLabelText('foresight_min_support').value).toBe('100');
    expect(within(dialog).getByLabelText('foresight_after_steps').value).toBe('5');
    fireEvent.change(within(dialog).getByLabelText('max_cost_usd'), { target: { value: '3' } });
    await act(async () => { fireEvent.click(within(dialog).getByText('Save policy')); });
    expect(api.savePolicy).toHaveBeenCalledWith(expect.objectContaining({
      name: 'support',
      limits: { max_cost_usd: 3, min_success_odds: 0.7, foresight_min_support: 100, foresight_after_steps: 5 },
    }));
    api.policies.mockImplementation(async () => policies);
  });

  it('refuses the companions without the odds they tune, as the server does, and only in a policy', () => {
    expect(parseLimits({ foresight_min_support: '100' }, { policy: true }).error)
      .toMatch(/foresight_min_support .*min_success_odds/);
    expect(parseLimits({ foresight_after_steps: '2.5', min_success_odds: '0.7' }, { policy: true }).error).toMatch(/whole/);
    expect(parseLimits({ min_success_odds: '0.7', foresight_min_support: '100' }, { policy: true }))
      .toEqual({ limits: { min_success_odds: 0.7, foresight_min_support: 100 } });
    // A grant may raise one alone: the server merges it over the policy's.
    expect(parseLimits({ foresight_min_support: '100' })).toEqual({ limits: { foresight_min_support: 100 } });
  });
});
