// @vitest-environment jsdom
//
// The Foresight screen, rendered from GET /v1/fate/cohorts, /v1/fate/map and
// /v1/fate/runs/{run} as a 0.28.0 server answered them over the behaviour
// corpus (`seed --behaviours`) and the imported Claude Code fixture.

import React from 'react';
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, act, cleanup, fireEvent, screen } from '@testing-library/react';
import cohorts from '../test/fixtures/insight/fate_cohorts.json';
import supportMap from '../test/fixtures/insight/fate_map_support.json';
import researcherMap from '../test/fixtures/insight/fate_map_researcher.json';
import doomed from '../test/fixtures/insight/fate_run_doomed.json';
import healthy from '../test/fixtures/insight/fate_run_healthy.json';
import novel from '../test/fixtures/insight/fate_run_novel.json';
import missing from '../test/fixtures/insight/fate_map_missing.json';
import ccPruned from '../test/fixtures/insight/fate_map_cc_pruned.json';

const RUNS = { 'live-doomed-0': doomed, 'live-healthy-0': healthy, 'live-novel-0': novel };

vi.mock('../lib/api.js', () => ({
  api: {
    fateCohorts: vi.fn(async () => cohorts),
    fateMap: vi.fn(async (params) => {
      if (params.agent === 'researcher') return researcherMap;
      if (params.agent === 'support-agent') return supportMap;
      if (params.agent === 'claude-code') return ccPruned;
      throw Object.assign(new Error(missing.error), { status: 404, what: missing.error, body: missing });
    }),
    fateRun: vi.fn(async (run) => RUNS[run]),
  },
}));

const { ForesightScreen } = await import('./ForesightScreen.jsx');
const { api } = await import('../lib/api.js');

const OPEN = { auth: 'open', scope: 'admin', operator: true, tenant: null };

async function show(query = '') {
  const go = vi.fn();
  const view = render(<ForesightScreen go={go} params={new URLSearchParams(query)} identity={OPEN} pushToast={vi.fn()} />);
  await act(async () => { for (let i = 0; i < 6; i += 1) await Promise.resolve(); });
  return { view, go };
}

const node = (path) => document.querySelector(`[data-testid="fate-node"][data-key="${path.join('\u0001')}"]`);

beforeEach(() => Object.values(api).forEach((fn) => fn.mockClear()));
afterEach(() => { cleanup(); window.history.replaceState(null, '', '#'); });

describe('the cohorts and the map', () => {
  it('lists every cohort, opens the largest, and asks for the default window by sending none', async () => {
    await show();
    const cards = screen.getAllByTestId('cohort').map((c) => c.textContent);
    expect(cards).toHaveLength(cohorts.cohorts.length);
    expect(cards[0]).toContain('support-agent');
    expect(cards[0]).toContain('180 runs');
    expect(cards[0]).toContain('+6 live');
    expect(cards[1]).toContain('task dd8fe599');
    expect(api.fateCohorts).toHaveBeenCalledWith({ since: undefined }, expect.anything());
    expect(api.fateMap).toHaveBeenCalledWith(expect.objectContaining({ agent: 'support-agent', depth: 12, min_support: 3, tenant: '' }), expect.anything());
    expect(screen.getByTestId('tile-runs').textContent).toContain('180');
  });

  it('draws every node, sized by runs, with fatal forks and hotspots named in words', async () => {
    await show('agent=support-agent');
    expect(screen.getAllByTestId('fate-node')).toHaveLength(supportMap.nodes_shown);
    const loop = node(['llm:gpt-4o', 'tool:search_orders×3-5']);
    expect(loop.textContent).toContain('FATAL FORK p <0.001');
    expect(loop.textContent).toContain('10.0%');
    expect(loop.getAttribute('aria-label')).toMatch(/50 runs, success 10\.0%, 95% interval 4\.3–21\.4%, fatal fork/);
    expect(node(['llm:gpt-4o']).textContent).toContain('HOTSPOT 1.50 bits');
    expect(screen.getAllByTestId('fatal-fork')).toHaveLength(2);
  });

  it('reads a hovered node: its interval, support, and cost and duration means', async () => {
    await show('agent=support-agent');
    fireEvent.mouseMove(node(['llm:gpt-4o', 'tool:search_orders×3-5']), { clientX: 10, clientY: 10 });
    const tip = screen.getByRole('tooltip').textContent;
    expect(tip).toContain('4.3–21.4%');
    expect(tip).toContain('5 / 40 / 5');
    expect(tip).toContain('$0.00400'); // 0.2 over 50 runs
    expect(tip).toContain('5.00 s'); // 249.849 s over 50 runs
    expect(tip).toContain('71.5% of 130');
  });

  it('opens a clicked node with its example runs, a diff of a success against a failure, and the TQL that finds them', async () => {
    await show('agent=support-agent');
    fireEvent.click(node(['llm:gpt-4o', 'tool:search_orders×3-5']));
    const params = new URLSearchParams(window.location.hash.split('?')[1]);
    expect(params.get('node')).toBe('llm:gpt-4o\u0001tool:search_orders×3-5');
    cleanup();
    const { go } = await show('agent=support-agent&node=' + encodeURIComponent('llm:gpt-4o\u0001tool:search_orders×3-5'));
    const panel = screen.getByTestId('node-panel');
    expect(panel.textContent).toContain('search_loop-9');
    expect(panel.textContent).toContain('p(failure | here)80.0%');
    fireEvent.click(screen.getByText('Diff success vs failure'));
    expect(go).toHaveBeenCalledWith(['diff'], { a: 'search_loop-9', b: 'search_loop-0', tenant: '' });
    fireEvent.click(screen.getByText('Find these runs'));
    expect(go).toHaveBeenCalledWith(['behaviors'], { q: '^llm("gpt-4o") -> tool("search_orders"){3,5}' });
    fireEvent.click(screen.getByText('search_loop-0'));
    expect(go).toHaveBeenCalledWith(['run', 'search_loop-0'], { tenant: '' });
  });

  it("states the map's own score, and what it means", async () => {
    await show('agent=support-agent');
    const card = screen.getByTestId('calibration').textContent;
    expect(card).toContain('much better than the base rate');
    expect(card).toContain('31%');
    expect(card).toContain('0.173'); // depth 3 Brier
  });

  it('asks again for a wider window and a stricter support', async () => {
    await show('agent=support-agent&window=90d&min=10');
    const asked = api.fateMap.mock.calls[0][0];
    expect(asked.since).toMatch(/^\d{19}$/);
    expect(asked.min_support).toBe(10);
    expect(api.fateCohorts.mock.calls[0][0].since).toBe(asked.since);
  });

  it('says why a map draws nothing past the start: every branch is under the support', async () => {
    await show('agent=claude-code&window=1y');
    expect(screen.getAllByTestId('fate-node')).toHaveLength(1);
    expect(screen.getByTestId('all-pruned').textContent)
      .toBe('Every branch from the start has fewer than 3 runs, so none is drawn (1 branch, 2 runs). Lower min support to see them.');
  });

  it('says so when the agent has no runs in the window', async () => {
    await show('agent=nobody');
    expect(screen.getByRole('alert').textContent).toContain('No runs of nobody in this window.');
  });
});

describe('a run on its map', () => {
  it('lights a live run up to its open last step, pooled, with what earlier runs did next', async () => {
    await show('run=live-healthy-0');
    // The map follows the run's agent.
    expect(api.fateMap).toHaveBeenCalledWith(expect.objectContaining({ agent: 'support-agent' }), expect.anything());
    const banner = screen.getByTestId('run-foresight').textContent;
    expect(banner).toContain('on track');
    expect(banner).toContain('runs that took this path succeeded 69 of 90 times');
    expect(banner).toContain('last step open, pooled');
    expect(screen.getByTestId('next-options').textContent).toMatch(/llm:gpt-4orecommended/);
    const refund = ['llm:gpt-4o', 'tool:search_orders', 'llm:gpt-4o', 'tool:issue_refund'];
    expect(node(refund).textContent).toContain('THIS RUN');
    expect(node([...refund, 'llm:gpt-4o']).textContent).toContain('RECOMMENDED NEXT');
    // The run's node is the one opened.
    expect(screen.getByTestId('node-panel').textContent).toContain('runs through here60');
  });

  it('names a likely failure', async () => {
    await show('run=live-doomed-0');
    const banner = screen.getByTestId('run-foresight').textContent;
    expect(banner).toContain('likely failure');
    expect(banner).toContain('5 of 50 times (10.0%, 95% CI 4.3–21.4%)');
    expect(node(['llm:gpt-4o', 'tool:search_orders×3-5']).textContent).toContain('THIS RUN');
  });

  it('draws where a novel run left the known paths, and what the runs that got there did instead', async () => {
    await show('run=live-novel-0');
    expect(screen.getByTestId('run-foresight').textContent).toContain('novel');
    expect(document.body.textContent).toContain('NEVER TAKEN HERE BEFORE');
    expect(document.body.textContent).toContain('delete_database');
    expect(node(['llm:gpt-4o', 'tool:search_orders']).textContent).toContain('RECOMMENDED NEXT');
  });

  it('does not light a run on one task of its agent, which is not what it was judged against', async () => {
    api.fateMap.mockResolvedValueOnce({ ...supportMap, task: 'dd8fe599d8750716' });
    await show('run=live-healthy-0&agent=support-agent&task=dd8fe599d8750716');
    expect(screen.getByTestId('run-foresight').textContent).toContain('The map shown is one task of support-agent');
    expect(document.body.textContent).not.toContain('THIS RUN');
  });

  it('says when the run belongs to another cohort than the map shown', async () => {
    await show('run=live-healthy-0&agent=researcher');
    expect(screen.getByTestId('run-foresight').textContent).toContain('This run belongs to support-agent; the map shown is researcher.');
    expect(document.body.textContent).not.toContain('THIS RUN');
  });
});

describe('no cohorts', () => {
  it('says what a cohort is, and how to see a map', async () => {
    api.fateCohorts.mockResolvedValueOnce({ ...cohorts, cohorts: [] });
    await show();
    expect(document.body.textContent).toContain('No agent has completed runs in this window');
    expect(document.body.textContent).toContain('seed --data-dir ./data --behaviours');
    expect(api.fateMap).not.toHaveBeenCalled();
  });
});
