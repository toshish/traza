// @vitest-environment jsdom
//
// The Changes screen, rendered from GET /v1/changes as a 0.28.0 server
// answered it over the behaviour corpus and the insight corpus's support
// bot, whose v1.5.0 deploy edited one word of its prompt.

import React from 'react';
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, act, cleanup, fireEvent, screen, within } from '@testing-library/react';
import changes from '../test/fixtures/insight/changes.json';
import placed from '../test/fixtures/insight/changes_run.json';
// Two agents posted to a fresh server: `tuned-bot`'s sampling parameters
// changed type (`max_tokens` "1024" → 1024), and `triage-bot`'s prompt edit
// moved success from 8 of 8 ended runs to 0 of 8, every other run declaring
// an outcome Traza does not recognize.
import typed from '../test/fixtures/insight/changes_params.json';
// A store of its own: support-agent in tenants acme and globex, each with a
// service.version change (globex's a regression), read in the operator view.
import twoTenants from '../test/fixtures/insight/changes_tenants.json';

vi.mock('../lib/api.js', () => ({
  api: {
    changes: vi.fn(async (params) => (params.run ? placed : changes)),
  },
}));

const { ChangesScreen, regimes } = await import('./ChangesScreen.jsx');
const { api } = await import('../lib/api.js');

const OPEN = { auth: 'open', scope: 'admin', operator: true, tenant: null };

async function show(query = '') {
  const go = vi.fn();
  const view = render(<ChangesScreen go={go} params={new URLSearchParams(query)} identity={OPEN} pushToast={vi.fn()} />);
  await act(async () => { for (let i = 0; i < 4; i += 1) await Promise.resolve(); });
  return { view, go };
}

beforeEach(() => { api.changes.mockClear(); });
afterEach(() => { cleanup(); window.history.replaceState(null, '', '#'); });

describe('the timeline', () => {
  it('lists every agent, regressions first, each with its summary', async () => {
    await show();
    expect(api.changes).toHaveBeenCalledWith({ since: undefined, environment: undefined, run: undefined }, expect.anything());
    const agents = screen.getAllByTestId('agent-changes');
    expect(agents).toHaveLength(4);
    expect(agents[0].textContent).toContain('support-bot: regression at 2026-10-17T06:34:56Z');
    expect(document.body.textContent).toContain('2 regressions in this window');
  });

  it('marks the change points, and tints the stretch a regression set up', async () => {
    await show('agent=support-bot');
    expect(screen.getAllByTestId('agent-changes')).toHaveLength(1);
    expect(screen.getAllByTestId('change-marker').map((m) => m.textContent)).toEqual(['system prompt', 'service version']);
    const stretches = screen.getAllByTestId('regime').map((r) => r.textContent);
    // Named by the service version, not the prompt's digest; each rate with
    // the runs it counts.
    expect(stretches).toEqual(['1.4.2err 5% of 60', '1.5.0err 40% of 60']);
  });

  it('splits an agent into regimes at its change points', () => {
    const bot = changes.agents[0];
    const r = regimes(bot);
    expect(r).toHaveLength(2);
    expect(r[0]).toMatchObject({ from: bot.first_run_ns, verdict: null, error: 0.05, n: 60 });
    expect(r[1]).toMatchObject({ to: bot.last_run_ns, verdict: 'regression', label: '1.5.0', error: 0.4, n: 60 });
    expect(regimes(changes.agents[2])).toEqual([]);
  });

  it('labels no stretch with a rate when unlisted change points may have cut its sides', () => {
    // Past 40 changes the lowest-ranked are not listed, yet each still
    // bounds its neighbours' sides: a drawn stretch may hold several.
    const r = regimes({ ...changes.agents[0], truncated: true });
    expect(r.map((s) => s.label)).toEqual(['1.4.2', '1.5.0']);
    expect(r.map((s) => s.error)).toEqual([null, null]);
  });
});

describe('a change', () => {
  it('shows the prompt edit itself, the deltas with p-values, and the runs either side', async () => {
    const { go } = await show('agent=support-bot');
    const card = screen.getAllByTestId('change')[0];
    expect(card.textContent).toContain('regression');
    expect(card.textContent).toContain('system prompt changed');
    expect(card.textContent).toContain('together with service version');
    expect(card.textContent).toContain('triage support tickets. Always→Never@ byte 28');
    expect(card.textContent).toMatch(/error rate5\.0%40\.0%<0\.001/);
    expect(card.textContent).toMatch(/success rate95\.0%60\.0%<0\.001/);
    expect(card.textContent).toContain('the agent took the same paths on both sides');
    fireEvent.click(screen.getAllByText('Diff the last run before against the first after')[0]);
    expect(go).toHaveBeenCalledWith(['diff'], { a: 'ticket-059', b: 'ticket-060', tenant: '' });
    fireEvent.click(screen.getAllByText('ticket-061')[0]);
    expect(go).toHaveBeenCalledWith(['run', 'ticket-061'], { tenant: '' });
  });

  it('shows a behaviour change by the paths whose share moved', async () => {
    await show('agent=support-agent');
    const card = screen.getAllByTestId('change')[0].textContent;
    expect(card).toContain('behaviour changed');
    expect(card).toContain('improvement');
    expect(card).toContain('efficiency');
    expect(card).toContain('paths shifted by 1.00');
  });
});

describe('what a change is judged on', () => {
  it('shows parameters as typed JSON, so a number and its text read apart', async () => {
    api.changes.mockResolvedValueOnce(typed);
    await show('agent=tuned-bot');
    const card = screen.getAllByTestId('change')[0].textContent;
    expect(card).toContain('max_tokens "1024" → 1024');
    expect(card).toContain('stop "END, temperature=0.7" → "END"');
    expect(card).toContain('temperature ∅ → 0.7');
  });

  it('never marks a metric whose own test had fewer than ten observations a side', async () => {
    api.changes.mockResolvedValueOnce(typed);
    await show('agent=triage-bot');
    const card = screen.getAllByTestId('change')[0];
    expect(card.textContent).toContain('neutral');
    const cells = [...card.querySelectorAll('[role="cell"]')];
    const at = cells.findIndex((c) => c.textContent === 'success rate');
    expect(cells[at + 4].textContent).toBe('8/8');
    // p = 0.00016 by Fisher's test, on 8 runs a side: shown, not marked.
    const after = cells[at + 2].querySelector('span');
    expect(after.style.color).toBe('var(--ink)');
    expect(cells[at + 4].title).toMatch(/not part of the verdict/);
  });
});

describe('one run on its timeline', () => {
  it('places the run and lists the changes it ran downstream of', async () => {
    await show('run=ticket-076');
    expect(api.changes).toHaveBeenCalledWith(expect.objectContaining({ run: 'ticket-076' }), expect.anything());
    const card = screen.getByTestId('placed-run').textContent;
    expect(card).toContain('ticket-076');
    expect(card).toContain('failure');
    expect(card).toContain('errored');
    expect(card).toMatch(/system prompt → f75dfaea2e7cfb71/);
    expect(card).toMatch(/service version → 1\.5\.0/);
  });

  it('takes the run to place from its field', async () => {
    await show();
    fireEvent.change(screen.getByLabelText('Run to place'), { target: { value: 'ticket-076' } });
    fireEvent.keyDown(screen.getByLabelText('Run to place'), { key: 'Enter' });
    expect(new URLSearchParams(window.location.hash.split('?')[1]).get('run')).toBe('ticket-076');
  });
});

describe('one agent name in two tenants', () => {
  it('jumps from a timeline marker to its own tenant\'s change, never the other\'s', async () => {
    api.changes.mockResolvedValueOnce(twoTenants);
    const scrolled = [];
    const before = Element.prototype.scrollIntoView;
    Element.prototype.scrollIntoView = function scrollIntoView() { scrolled.push(this); };
    try {
      await show();
      const sections = screen.getAllByTestId('agent-changes');
      expect(sections.map((s) => s.textContent.match(/tenant ([a-z]+)/)[1])).toEqual(['globex', 'acme']);
      const cards = screen.getAllByTestId('change');
      expect(new Set(cards.map((card) => card.id)).size).toBe(cards.length);
      for (const section of sections) {
        fireEvent.click(within(section).getByTestId('change-marker'));
        expect(scrolled[scrolled.length - 1]).toBe(within(section).getByTestId('change'));
      }
    } finally {
      Element.prototype.scrollIntoView = before;
    }
  });
});
