// @vitest-environment jsdom
//
// The Run screen, rendered from GET /v1/tapes/{run} and POST /v1/replays as
// a 0.27.0 server answered them for the weather-9 run (fixtures).

import React from 'react';
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, act, cleanup, fireEvent, screen } from '@testing-library/react';
import tape from '../test/fixtures/tape_weather9.json';
import replay from '../test/fixtures/replay.json';

vi.mock('../lib/api.js', () => ({
  api: {
    tape: vi.fn(async () => tape),
    fuseRun: vi.fn(async () => ({ verdict: 'continue' })),
    sessions: vi.fn(async () => ({ sessions: [{ session_id: 'weather-7', tenant: '' }] })),
    createReplay: vi.fn(async () => replay),
    replay: vi.fn(async () => ({ served: 1, remaining: 1, diverged: null })),
    payload: vi.fn(async () => '{"k":1}'),
    // The foresight panel's read; this file is about the tape.
    fateRun: vi.fn(async () => { throw Object.assign(new Error('run not found'), { status: 404, what: 'run not found' }); }),
  },
}));

const { RunScreen } = await import('./RunScreen.jsx');
const { api } = await import('../lib/api.js');

const OPEN = { auth: 'open', scope: 'admin', operator: true, tenant: null };
const BOUND = { auth: 'token', scope: 'rw', operator: false, tenant: 'acme' };

async function show(params = new URLSearchParams(), runId = 'weather-9', identity = OPEN) {
  const go = vi.fn();
  const view = render(<RunScreen runId={runId} go={go} params={params} identity={identity} pushToast={vi.fn()} />);
  await act(async () => { for (let i = 0; i < 4; i += 1) await Promise.resolve(); });
  return { view, go };
}

beforeEach(() => Object.values(api).forEach((fn) => fn.mockClear()));
afterEach(cleanup);

describe('the tape', () => {
  it('lists the steps in order with kind, name, duration, tokens and cost', async () => {
    await show();
    const rows = screen.getAllByTestId('tape-step').map((node) => node.textContent);
    expect(rows).toHaveLength(4);
    expect(rows[0]).toMatch(/^0llmgpt-4o-2024-08-06openai\.chat · weather-agent/);
    expect(rows[0]).toContain('390.00 ms');
    expect(rows[0]).toContain('82 / 17');
    expect(rows[0]).toContain('0.00038');
    expect(rows[1]).toMatch(/^1toolget_weather/);
    expect(rows[2]).toMatch(/^2toolget_air_quality/);
    expect(rows[3]).toContain('gpt-4o-mini-2024-07-18');
    expect(api.tape).toHaveBeenCalledWith('weather-9', expect.objectContaining({ max_steps: 2000 }), expect.anything());
  });

  it('opens a step to show its request and response, rendered by kind', async () => {
    await show();
    const [first, second] = screen.getAllByTestId('tape-step');
    fireEvent.click(first.querySelector('[role="row"]'));
    expect(first.textContent).toContain("What's the weather in Paris?");
    expect(first.textContent).toContain('get_weather');
    expect(first.textContent).toContain('sha256 8b9a07d4aac0');
    fireEvent.click(second.querySelector('[role="row"]'));
    expect(second.textContent).toContain('"celsius": 12');
  });

  it('opens the step a diff linked to', async () => {
    await show(new URLSearchParams('step=2'));
    const steps = screen.getAllByTestId('tape-step');
    // Only step 2 is open: its digests show, the others' do not.
    expect(steps[2].textContent).toContain('fingerprint tool:get_air_quality');
    expect(steps[1].textContent).not.toContain('fingerprint');
  });

  it('asks the server to inline payloads when told to', async () => {
    await show(new URLSearchParams('resolve=true'));
    expect(api.tape).toHaveBeenCalledWith('weather-9', expect.objectContaining({ resolve: 'true' }), expect.anything());
  });
});

describe('the tenant the tape is read in', () => {
  it('reads a default-tenant run in the default tenant, named explicitly, on every route', async () => {
    const { go } = await show(new URLSearchParams('tenant='));
    // `tenant=` is the default tenant on tapes, diffs and replays; leaving it
    // out is the operator view across every tenant.
    expect(api.tape).toHaveBeenCalledWith('weather-9', expect.objectContaining({ tenant: '' }), expect.anything());
    expect(api.fuseRun).toHaveBeenCalledWith('weather-9', { tenant: '' }, expect.anything());
    expect(screen.getByText('guard: continue')).toBeTruthy();
    await act(async () => { fireEvent.click(screen.getByText('Create replay')); });
    expect(api.createReplay).toHaveBeenCalledWith({ run: 'weather-9', mode: 'strict', tenant: '' });
    fireEvent.change(screen.getByLabelText('Run to compare with'), { target: { value: 'weather-7' } });
    fireEvent.click(screen.getByText('Compare'));
    expect(go).toHaveBeenCalledWith(['diff'], { a: 'weather-9', b: 'weather-7', tenant: '' });
  });

  it('tells an operator an unscoped tape spans every tenant, and shows no verdict for a scope it does not share', async () => {
    const { view } = await show(new URLSearchParams());
    expect(api.tape.mock.calls[0][1]).not.toHaveProperty('tenant');
    expect(view.container.textContent).toMatch(/every tenant/);
    expect(view.container.textContent).not.toContain('guard:');
    expect(api.fuseRun).not.toHaveBeenCalled();
  });

  it('lets an operator scope an unscoped tape to the default tenant', async () => {
    await show(new URLSearchParams());
    const before = window.location.hash;
    fireEvent.click(screen.getByText('Scope'));
    expect(window.location.hash).toBe('#/run/weather-9?tenant=');
    window.history.replaceState(null, '', before || '#');
  });

  it('shows a bound credential its verdict without naming a tenant: the server reads its own', async () => {
    const { view } = await show(new URLSearchParams(), 'weather-9', BOUND);
    expect(api.tape.mock.calls[0][1]).not.toHaveProperty('tenant');
    expect(api.fuseRun).toHaveBeenCalledWith('weather-9', undefined, expect.anything());
    expect(view.container.textContent).not.toMatch(/every tenant/);
    expect(screen.getByText('guard: continue')).toBeTruthy();
  });
});

describe('a step past the first page', () => {
  it('asks for enough steps to include the step a diff linked to', async () => {
    await show(new URLSearchParams('tenant=&step=3500'));
    expect(api.tape).toHaveBeenCalledWith('weather-9', expect.objectContaining({ max_steps: 4000 }), expect.anything());
  });

  it('says so when the linked step is not on the tape at all', async () => {
    await show(new URLSearchParams('tenant=&step=9'));
    expect(screen.getByTestId('missing-step').textContent).toMatch(/step 9.*4 steps/i);
  });
});

describe('replay this run', () => {
  it('creates a replay and hands over both SDKs’ environment and a Python example', async () => {
    await show();
    await act(async () => { fireEvent.click(screen.getByText('Create replay')); });
    expect(api.createReplay).toHaveBeenCalledWith({ run: 'weather-9', mode: 'strict' });
    const panel = screen.getByTestId('replay-ready').textContent;
    const base = window.location.origin;
    expect(panel).toContain(`export OPENAI_BASE_URL=${base}${replay.base_urls.openai}`);
    expect(panel).toContain(`export ANTHROPIC_BASE_URL=${base}${replay.base_urls.anthropic}`);
    expect(panel).toContain('OPENAI_API_KEY=');
    expect(panel).toContain('ANTHROPIC_API_KEY=');
    expect(panel).toContain('client.chat.completions.create(model="gpt-4o-2024-08-06"');
  });

  it('sends a loose replay of the run in its tenant when asked', async () => {
    await show(new URLSearchParams('tenant=acme'));
    fireEvent.click(screen.getByText('loose'));
    await act(async () => { fireEvent.click(screen.getByText('Create replay')); });
    expect(api.createReplay).toHaveBeenCalledWith({ run: 'weather-9', mode: 'loose', tenant: 'acme' });
  });
});

describe('replay this run, when the tape changes scope', () => {
  // Scoping an unscoped tape to a tenant reads a different recording. A
  // replay made of the old one, or still being made, describes that one: its
  // environment lines beside the new tape's Python would hand over two
  // recordings as one set of instructions.
  const ACME = { ...replay, id: 'rp_acme_scoped' };
  const rerenderWith = async (view, params) => {
    view.rerender(<RunScreen runId="weather-9" go={vi.fn()} params={params} identity={OPEN} pushToast={vi.fn()} />);
    await act(async () => { for (let i = 0; i < 4; i += 1) await Promise.resolve(); });
  };

  it('drops a replay made in the old scope', async () => {
    const { view } = await show(new URLSearchParams());
    await act(async () => { fireEvent.click(screen.getByText('Create replay')); });
    expect(screen.getByTestId('replay-ready').textContent).toContain(replay.id);

    await rerenderWith(view, new URLSearchParams('tenant=acme'));
    expect(api.tape).toHaveBeenLastCalledWith('weather-9', expect.objectContaining({ tenant: 'acme' }), expect.anything());
    expect(screen.queryByTestId('replay-ready')).toBeNull();
    expect(view.container.textContent).not.toContain(replay.id);

    // A replay made now is made in the new scope.
    api.createReplay.mockResolvedValueOnce(ACME);
    await act(async () => { fireEvent.click(screen.getByText('Create replay')); });
    expect(api.createReplay).toHaveBeenLastCalledWith({ run: 'weather-9', mode: 'strict', tenant: 'acme' });
    expect(screen.getByTestId('replay-ready').textContent).toContain(ACME.id);
  });

  it('discards a creation still pending when the scope changed', async () => {
    let finish;
    api.createReplay.mockImplementationOnce(() => new Promise((resolve) => { finish = resolve; }));
    const { view } = await show(new URLSearchParams());
    await act(async () => { fireEvent.click(screen.getByText('Create replay')); });
    expect(screen.getByText('Creating…')).toBeTruthy();

    await rerenderWith(view, new URLSearchParams('tenant=acme'));
    await act(async () => { finish(replay); for (let i = 0; i < 4; i += 1) await Promise.resolve(); });
    expect(screen.queryByTestId('replay-ready')).toBeNull();
    expect(view.container.textContent).not.toContain(replay.id);
    expect(screen.getByText('Create replay')).toBeTruthy();
  });

  it('stays with the tape on screen until the new scope’s tape arrives', async () => {
    const { view } = await show(new URLSearchParams());
    await act(async () => { fireEvent.click(screen.getByText('Create replay')); });
    let arrive;
    api.tape.mockImplementationOnce(() => new Promise((resolve) => { arrive = resolve; }));
    await rerenderWith(view, new URLSearchParams('tenant=acme'));

    // The unscoped tape is still the one shown, so its replay stays beside
    // it, and one made now is of that recording — the Python under it is.
    expect(screen.getByTestId('replay-ready').textContent).toContain(replay.id);
    await act(async () => { fireEvent.click(screen.getByText('Create another')); });
    expect(api.createReplay).toHaveBeenLastCalledWith({ run: 'weather-9', mode: 'strict' });

    await act(async () => { arrive(tape); for (let i = 0; i < 4; i += 1) await Promise.resolve(); });
    expect(screen.queryByTestId('replay-ready')).toBeNull();
  });
});

describe('compare with…', () => {
  it('goes to the diff of this run against the one named', async () => {
    const { go } = await show();
    fireEvent.change(screen.getByLabelText('Run to compare with'), { target: { value: 'weather-7' } });
    fireEvent.click(screen.getByText('Compare'));
    expect(go).toHaveBeenCalledWith(['diff'], { a: 'weather-9', b: 'weather-7' });
  });
});

describe('a run with no spans', () => {
  it('says what a run id is rather than showing an empty tape', async () => {
    api.tape.mockRejectedValueOnce(Object.assign(new Error('run not found'), { status: 404, what: 'run not found' }));
    await show(new URLSearchParams(), 'nope');
    expect(screen.getByRole('alert').textContent).toContain('No spans carry this run id.');
  });
});
