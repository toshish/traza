// @vitest-environment jsdom
//
// The Diff screen, rendered from GET /v1/diff as a 0.27.0 server answered it
// for weather-7 against weather-9: two identical steps, an inserted tool
// call, and a model swapped for a cheaper one (fixture).

import React from 'react';
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, act, cleanup, fireEvent, screen, within } from '@testing-library/react';
import diff from '../test/fixtures/diff_7_9.json';

vi.mock('../lib/api.js', () => ({
  api: { diff: vi.fn(async () => diff) },
}));

const { DiffScreen } = await import('./DiffScreen.jsx');
const { api } = await import('../lib/api.js');

async function show(query) {
  const go = vi.fn();
  const view = render(<DiffScreen go={go} params={new URLSearchParams(query)} />);
  await act(async () => { for (let i = 0; i < 4; i += 1) await Promise.resolve(); });
  return { view, go };
}

beforeEach(() => api.diff.mockClear());
afterEach(cleanup);

describe('two runs aligned', () => {
  it('asks the server for the pair, in the tenant named', async () => {
    await show('a=weather-7&b=weather-9&tenant=acme');
    expect(api.diff).toHaveBeenCalledWith({ a: 'weather-7', b: 'weather-9', tenant: 'acme' }, expect.anything());
  });

  it('draws every aligned step with its op, and highlights the first divergence', async () => {
    await show('a=weather-7&b=weather-9');
    const rows = screen.getAllByTestId('aligned-row');
    expect(rows.map((row) => row.dataset.op)).toEqual(['same', 'same', 'insert', 'changed']);
    const first = rows.filter((row) => row.dataset.first === 'true');
    expect(first).toHaveLength(1);
    expect(first[0].dataset.op).toBe('insert');
    expect(first[0].textContent).toContain('tool:get_air_quality');
    expect(first[0].textContent).toContain('structure');
    // The model swap is the last row: same signature kind, different model.
    expect(rows[3].textContent).toContain('gpt-4o-mini-2024-07-18');
    expect(rows[3].textContent).toContain('config');
  });

  it('states the first divergence: its kind, what that means, and the explanation', async () => {
    await show('a=weather-7&b=weather-9');
    const card = screen.getByTestId('first-divergence');
    expect(card.textContent).toContain('structure');
    expect(card.textContent).toContain('a step only one run has');
    expect(card.textContent).toContain('run b has step 2 (tool:get_air_quality) where run a has no matching step');
  });

  it('shows the totals and their delta', async () => {
    await show('a=weather-7&b=weather-9');
    expect(screen.getByTestId('total-steps').textContent).toBe('steps34+1');
    expect(screen.getByTestId('total-tokens').textContent).toBe('tokens2332330');
    expect(screen.getByTestId('total-outcome').textContent).toContain('success');
  });

  it("links a step to its run's tape, opened at that step", async () => {
    const { go } = await show('a=weather-7&b=weather-9');
    const changed = screen.getAllByTestId('aligned-row')[3];
    fireEvent.click(within(changed).getByText('3'));
    expect(go).toHaveBeenCalledWith(['run', 'weather-9'], { step: '3' });
  });
});

describe('without two runs', () => {
  it('asks for them', async () => {
    const { go } = await show('a=weather-7');
    expect(api.diff).not.toHaveBeenCalled();
    fireEvent.change(screen.getByLabelText('Run b'), { target: { value: 'weather-8' } });
    fireEvent.click(screen.getByText('Compare'));
    expect(go).toHaveBeenCalledWith(['diff'], { a: 'weather-7', b: 'weather-8' });
  });
});
