// @vitest-environment jsdom
//
// The Guard screen, rendered from GET /v1/fuses/runs and GET
// /v1/fuses/policies exactly as a 0.27.0 server answered them (fixtures),
// with the kill switch and the reset driven through their dialogs.

import React from 'react';
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, act, cleanup, fireEvent, screen, within } from '@testing-library/react';
import fuseRuns from '../test/fixtures/fuse_runs.json';
import policies from '../test/fixtures/policies.json';

vi.mock('../lib/api.js', () => ({
  api: {
    fuseRuns: vi.fn(async () => fuseRuns),
    policies: vi.fn(async () => policies),
    fuseTrip: vi.fn(async (run) => ({ ...fuseRuns.runs[0], run, verdict: 'stop', tripped_by: 'manual' })),
    fuseReset: vi.fn(async (run) => ({ ...fuseRuns.runs[0], run, verdict: 'continue' })),
    savePolicy: vi.fn(async (body) => ({ ...body, id: 'fp_1', tenant: '' })),
    deletePolicy: vi.fn(async () => ({ deleted: true })),
  },
}));

const { GuardScreen, parseLimits } = await import('./GuardScreen.jsx');
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
    // The default tenant is named by naming none.
    expect(api.fuseReset).toHaveBeenCalledWith('sess-7f3a', { max_repeats: 8 }, undefined);
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
