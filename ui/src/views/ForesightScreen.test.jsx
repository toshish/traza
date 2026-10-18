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
import supportDepth2 from '../test/fixtures/insight/fate_map_support_depth2.json';
import miniMap from '../test/fixtures/insight/fate_map_mini.json';

const RUNS = { 'live-doomed-0': doomed, 'live-healthy-0': healthy, 'live-novel-0': novel };

vi.mock('../lib/api.js', () => ({
  api: {
    fateCohorts: vi.fn(async () => cohorts),
    fateMap: vi.fn(async (params) => {
      if (params.agent === 'researcher') return researcherMap;
      if (params.agent === 'support-agent') return params.depth === 2 ? supportDepth2 : supportMap;
      if (params.agent === 'claude-code') return ccPruned;
      throw Object.assign(new Error(missing.error), { status: 404, what: missing.error, body: missing });
    }),
    fateRun: vi.fn(async (run) => RUNS[run]),
  },
}));

const { ForesightScreen } = await import('./ForesightScreen.jsx');
const { api } = await import('../lib/api.js');

const OPEN = { auth: 'open', scope: 'admin', operator: true, tenant: null };

async function show(query = '', identity = OPEN) {
  const go = vi.fn();
  const view = render(<ForesightScreen go={go} params={new URLSearchParams(query)} identity={identity} pushToast={vi.fn()} />);
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
    // The search is the cohort's: its agent, in its tenant.
    fireEvent.click(screen.getByText('Find these runs'));
    expect(go).toHaveBeenCalledWith(['behaviors'], {
      q: '^llm[signature="llm:gpt-4o"] -> tool[signature="tool:search_orders"]{3,5}$'
        + ' | ^llm[signature="llm:gpt-4o"] -> tool[signature="tool:search_orders"]{3,5} -> step[signature!="tool:search_orders"]',
      agent: 'support-agent', tenant: '',
    });
    fireEvent.click(screen.getByText('search_loop-0'));
    expect(go).toHaveBeenCalledWith(['run', 'search_loop-0'], { tenant: '' });
  });

  it("says a task map's search covers every task of the agent", async () => {
    api.fateMap.mockResolvedValueOnce({ ...supportMap, task: 'dd8fe599d8750716' });
    const { go } = await show('agent=support-agent&task=dd8fe599d8750716&node=' + encodeURIComponent('llm:gpt-4o'));
    expect(screen.getByTestId('node-panel').textContent).toContain('searches every task of support-agent');
    fireEvent.click(screen.getByText('Find these runs'));
    expect(go).toHaveBeenCalledWith(['behaviors'], {
      q: '^llm[signature="llm:gpt-4o"]$ | ^llm[signature="llm:gpt-4o"] -> step[signature!="llm:gpt-4o"]', agent: 'support-agent', tenant: '',
    });
  });

  it('offers no search for a node another branch reads the same as', async () => {
    // A tool literally named search×2, called once, beside search called twice.
    api.fateMap.mockResolvedValueOnce(miniMap);
    await show('agent=mini-agent&node=' + encodeURIComponent('llm:gpt-4o-mini\u0001tool:search×2'));
    expect(screen.getByTestId('node-panel').textContent).toContain('runs through here1');
    expect(screen.queryByText('Find these runs')).toBeNull();
  });

  it('opens the second of two branches that read the same, not the first', async () => {
    // `lit` called a tool named search×2 once; `two` called search twice.
    const errors = vi.spyOn(console, 'error').mockImplementation(() => {});
    api.fateMap.mockResolvedValueOnce(miniMap);
    await show('agent=mini-agent');
    const twins = [...document.querySelectorAll('[data-testid="fate-node"][data-token="tool:search×2"]')];
    expect(twins).toHaveLength(2);
    fireEvent.click(twins[1]);
    const key = new URLSearchParams(window.location.hash.split('?')[1]).get('node');
    expect(key).toBe(twins[1].getAttribute('data-key'));
    expect(key).not.toBe(twins[0].getAttribute('data-key'));
    cleanup();
    api.fateMap.mockResolvedValueOnce(miniMap);
    await show('agent=mini-agent&node=' + encodeURIComponent(key));
    const panel = screen.getByTestId('node-panel').textContent;
    expect(panel).toContain('two');
    expect(panel).not.toContain('lit');
    expect(panel).toContain('success / failure / abandoned0 / 1 / 0');
    // Its path reads as the map's text, with no key marker in it.
    expect(panel).not.toMatch(/[\u0001\u0002]/);
    const duplicate = errors.mock.calls.filter((args) => args.some((arg) => typeof arg === 'string' && /same key/i.test(arg)));
    expect(duplicate).toEqual([]);
    errors.mockRestore();
  });

  it("states the map's own score, and what it means", async () => {
    await show('agent=support-agent');
    const card = screen.getByTestId('calibration').textContent;
    expect(card).toContain('much better than the base rate');
    expect(card).toContain('31%');
    expect(card).toContain('0.173'); // depth 3 Brier
    // The headline says how many runs it was scored on.
    expect(screen.getByTestId('tile-skill').textContent).toContain('depth 3 · 180 runs');
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
    await show('run=live-healthy-0&tenant=');
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
    await show('run=live-doomed-0&tenant=');
    const banner = screen.getByTestId('run-foresight').textContent;
    expect(banner).toContain('likely failure');
    expect(banner).toContain('5 of 50 times (10.0%, 95% CI 4.3–21.4%)');
    expect(node(['llm:gpt-4o', 'tool:search_orders×3-5']).textContent).toContain('THIS RUN');
    // How far the map's verdicts at this depth have been worth trusting.
    expect(screen.getByTestId('run-skill').textContent).toContain('skill 21% at depth 2');
    expect(screen.getByTestId('run-skill').textContent).not.toContain('Do not act');
  });

  it('warns when the map has no skill at the depth the run was judged', async () => {
    await show('run=live-novel-0&tenant=');
    const skill = screen.getByTestId('run-skill').textContent;
    expect(skill).toContain('skill 0% at depth 1');
    expect(skill).toContain('no better than the base rate');
    expect(skill).toContain('Do not act on this verdict');
  });

  it('places nothing on a map cut shorter than where the run was judged, and offers to draw it', async () => {
    await show('run=live-healthy-0&tenant=&depth=2');
    expect(api.fateMap).toHaveBeenCalledWith(expect.objectContaining({ agent: 'support-agent', depth: 2 }), expect.anything());
    expect(document.body.textContent).not.toContain('THIS RUN');
    expect(document.body.textContent).not.toContain('RECOMMENDED NEXT');
    // The run's path is still lit as far as the map draws it, and that
    // node is the one opened.
    expect(screen.getByTestId('node-panel').textContent).toContain('runs through here90');
    const caveat = screen.getByTestId('run-past-map').textContent;
    expect(caveat).toContain('judged this run at depth 4');
    expect(caveat).toContain('the map shown draws 2 of its tokens');
    fireEvent.click(screen.getByText('Draw it'));
    const params = new URLSearchParams(window.location.hash.split('?')[1]);
    expect(params.get('depth')).toBe('6');
    expect(params.get('min')).toBeNull();
  });

  it('reads no foresight for an operator who names no tenant, which would read the default tenant', async () => {
    await show('run=live-healthy-0');
    expect(api.fateRun).not.toHaveBeenCalled();
    expect(screen.getByTestId('run-foresight').textContent).toContain('Foresight reads one tenant');
    // The map still opens.
    expect(api.fateMap).toHaveBeenCalled();
    expect(document.body.textContent).not.toContain('THIS RUN');
    // A credential bound to its tenant reads its own.
    cleanup();
    await show('run=live-healthy-0', { auth: 'token', scope: 'ro', operator: false, tenant: 'acme' });
    expect(api.fateRun).toHaveBeenCalledWith('live-healthy-0', undefined, expect.anything());
  });

  it('draws where a novel run left the known paths, and what the runs that got there did instead', async () => {
    await show('run=live-novel-0&tenant=');
    expect(screen.getByTestId('run-foresight').textContent).toContain('novel');
    expect(document.body.textContent).toContain('NEVER TAKEN HERE BEFORE');
    expect(document.body.textContent).toContain('delete_database');
    expect(node(['llm:gpt-4o', 'tool:search_orders']).textContent).toContain('RECOMMENDED NEXT');
  });

  it('does not light a run on one task of its agent, which is not what it was judged against', async () => {
    api.fateMap.mockResolvedValueOnce({ ...supportMap, task: 'dd8fe599d8750716' });
    await show('run=live-healthy-0&tenant=&agent=support-agent&task=dd8fe599d8750716');
    expect(screen.getByTestId('run-foresight').textContent).toContain('The map shown is one task of support-agent');
    expect(document.body.textContent).not.toContain('THIS RUN');
  });

  it('says when the run belongs to another cohort than the map shown', async () => {
    await show('run=live-healthy-0&tenant=&agent=researcher');
    expect(screen.getByTestId('run-foresight').textContent).toContain('This run belongs to support-agent; the map shown is researcher.');
    expect(document.body.textContent).not.toContain('THIS RUN');
  });
});

describe('when the route moves on while an answer is on its way', () => {
  const rerender = async (view, query) => {
    view.rerender(<ForesightScreen go={vi.fn()} params={new URLSearchParams(query)} identity={OPEN} pushToast={vi.fn()} />);
    await act(async () => { for (let i = 0; i < 6; i += 1) await Promise.resolve(); });
  };

  it('shows no verdict under the run now named until that run\'s own answer arrives', async () => {
    const { view } = await show('run=live-doomed-0&tenant=');
    expect(screen.getByTestId('run-foresight').textContent).toContain('likely failure');
    let arrive;
    api.fateRun.mockImplementationOnce(() => new Promise((resolve) => { arrive = resolve; }));
    await rerender(view, 'run=live-healthy-0&tenant=');
    expect(api.fateRun).toHaveBeenLastCalledWith('live-healthy-0', { tenant: '' }, expect.anything());
    // live-doomed-0's verdict, odds and path are not live-healthy-0's.
    expect(document.body.textContent).not.toContain('likely failure');
    expect(document.body.textContent).not.toContain('5 of 50 times');
    expect(document.body.textContent).not.toContain('THIS RUN');
    await act(async () => { arrive(healthy); for (let i = 0; i < 6; i += 1) await Promise.resolve(); });
    const banner = screen.getByTestId('run-foresight').textContent;
    expect(banner).toContain('live-healthy-0');
    expect(banner).toContain('on track');
  });

  it("opens the runs of the map still drawn in that map's tenant, not the tenant whose map is loading", async () => {
    const loop = encodeURIComponent('llm:gpt-4o\u0001tool:search_orders×3-5');
    const go = vi.fn();
    const view = render(<ForesightScreen go={go} params={new URLSearchParams('agent=support-agent&tenant=acme&node=' + loop)}
      identity={OPEN} pushToast={vi.fn()} />);
    await act(async () => { for (let i = 0; i < 6; i += 1) await Promise.resolve(); });
    expect(api.fateMap).toHaveBeenLastCalledWith(expect.objectContaining({ tenant: 'acme' }), expect.anything());
    let arrive;
    api.fateMap.mockImplementationOnce(() => new Promise((resolve) => { arrive = resolve; }));
    view.rerender(<ForesightScreen go={go} params={new URLSearchParams('agent=support-agent&tenant=beta&node=' + loop)}
      identity={OPEN} pushToast={vi.fn()} />);
    await act(async () => { for (let i = 0; i < 6; i += 1) await Promise.resolve(); });
    expect(api.fateMap).toHaveBeenLastCalledWith(expect.objectContaining({ tenant: 'beta' }), expect.anything());
    // acme's map is still drawn while beta's loads: its runs are acme's.
    expect(screen.getByTestId('node-panel').textContent).toContain('search_loop-0');
    fireEvent.click(screen.getByText('search_loop-0'));
    expect(go).toHaveBeenLastCalledWith(['run', 'search_loop-0'], { tenant: 'acme' });
    fireEvent.click(screen.getByText('Diff success vs failure'));
    expect(go).toHaveBeenLastCalledWith(['diff'], { a: 'search_loop-9', b: 'search_loop-0', tenant: 'acme' });
    // Once beta's map arrives, its runs are beta's.
    await act(async () => { arrive(supportMap); for (let i = 0; i < 6; i += 1) await Promise.resolve(); });
    fireEvent.click(screen.getByText('search_loop-0'));
    expect(go).toHaveBeenLastCalledWith(['run', 'search_loop-0'], { tenant: 'beta' });
  });

  it('reads the map\'s skill only off the build the run was judged against, not the one still drawn', async () => {
    const { view } = await show('run=live-doomed-0&tenant=&window=7d');
    // A 7-day map is not the build foresight judged the run against.
    expect(screen.queryByTestId('run-skill')).toBeNull();
    let arrive;
    api.fateMap.mockImplementationOnce(() => new Promise((resolve) => { arrive = resolve; }));
    await rerender(view, 'run=live-doomed-0&tenant=');
    // The 7-day map is still drawn while the default window's loads; its
    // calibration is not the one the verdict is read against.
    expect(screen.queryByTestId('run-skill')).toBeNull();
    await act(async () => { arrive(supportMap); for (let i = 0; i < 6; i += 1) await Promise.resolve(); });
    expect(screen.getByTestId('run-skill').textContent).toContain('skill 21% at depth 2');
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
