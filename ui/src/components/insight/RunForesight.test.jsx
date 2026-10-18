// @vitest-environment jsdom
//
// The Run screen's foresight panel and its X-Ray links, rendered from
// GET /v1/fate/runs/{run} as a 0.28.0 server answered it over the behaviour
// corpus and the imported Claude Code fixture.

import React from 'react';
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, act, cleanup, fireEvent, screen } from '@testing-library/react';
import novel from '../../test/fixtures/insight/fate_run_novel.json';
import doomed from '../../test/fixtures/insight/fate_run_doomed.json';
import cc from '../../test/fixtures/insight/fate_run_cc.json';
import tape from '../../test/fixtures/tape_weather9.json';

vi.mock('../../lib/api.js', () => ({
  api: {
    fateRun: vi.fn(async (run) => {
      if (run === 'live-novel-0') return novel;
      if (run === 'live-doomed-0') return doomed;
      if (run === 'weather-9' || run === 'gone') throw Object.assign(new Error('run not found'), { status: 404, what: 'run not found' });
      return cc;
    }),
    tape: vi.fn(async () => tape),
    fuseRun: vi.fn(async () => null),
    sessions: vi.fn(async () => ({ sessions: [] })),
  },
}));

const { RunForesight } = await import('./RunForesight.jsx');
const { RunScreen } = await import('../../views/RunScreen.jsx');
const { api } = await import('../../lib/api.js');

async function panel(runId, tenant) {
  const go = vi.fn();
  render(<RunForesight runId={runId} tenant={tenant} go={go} />);
  await act(async () => { for (let i = 0; i < 4; i += 1) await Promise.resolve(); });
  return go;
}

beforeEach(() => Object.values(api).forEach((fn) => fn.mockClear()));
afterEach(cleanup);

describe('the foresight panel', () => {
  it('says a novel run took a step no earlier run took, and what they did instead', async () => {
    const go = await panel('live-novel-0', '');
    expect(api.fateRun).toHaveBeenCalledWith('live-novel-0', { tenant: '' }, expect.anything());
    const text = screen.getByTestId('run-foresight-panel').textContent;
    expect(text).toContain('novel');
    expect(text).toContain('no earlier run of this agent took `tool:delete_database` at this point');
    expect(text).toContain('It took tool:delete_database where no earlier run did.');
    expect(text).toContain('matched 1 of 2 tokens');
    fireEvent.click(screen.getByText('On the Fate Map'));
    expect(go).toHaveBeenCalledWith(['foresight'], { run: 'live-novel-0', agent: 'support-agent', tenant: '' });
  });

  it('draws a likely failure against the base rate', async () => {
    await panel('live-doomed-0');
    const text = screen.getByTestId('run-foresight-panel').textContent;
    expect(text).toContain('likely failure');
    expect(text).toContain('10.0% 4.3–21.4%');
    expect(screen.getAllByRole('img', { name: /success 10\.0%, 95% interval 4\.3–21\.4%, base rate 54\.4%/ }).length).toBeGreaterThan(0);
  });

  it('shows a run with no history to compare against without inventing odds', async () => {
    await panel('11111111-1111-4111-8111-111111111111');
    const text = screen.getByTestId('run-foresight-panel').textContent;
    expect(text).toContain('insufficient history');
    expect(text).toContain('no completed runs of agent `claude-code` in the window to compare with');
    expect(screen.queryByRole('img')).toBeNull();
  });

  it('says so when the map holds no span of the run', async () => {
    await panel('gone');
    expect(screen.getByTestId('run-foresight-panel').textContent).toContain('No foresight: the Fate Map holds no span of this run.');
  });
});

describe('the Run screen', () => {
  it('opens a model call in the X-Ray, in the run\'s tenant, and the whole run from the header', async () => {
    const go = vi.fn();
    render(<RunScreen runId="weather-9" go={go} params={new URLSearchParams('tenant=')} identity={null} pushToast={vi.fn()} />);
    await act(async () => { for (let i = 0; i < 4; i += 1) await Promise.resolve(); });
    expect(screen.getByTestId('run-foresight-panel')).toBeTruthy();
    const [first, second] = screen.getAllByTestId('tape-step');
    fireEvent.click(first.querySelector('[role="row"]'));
    fireEvent.click(screen.getByTestId('xray-link'));
    expect(go).toHaveBeenCalledWith(['xray', 'weather-9'], { tenant: '', step: '0' });
    // A tool step has no context window to X-ray.
    fireEvent.click(second.querySelector('[role="row"]'));
    expect(screen.getAllByTestId('xray-link')).toHaveLength(1);
    fireEvent.click(screen.getByText('X-Ray'));
    expect(go).toHaveBeenCalledWith(['xray', 'weather-9'], { tenant: '' });
  });
});
