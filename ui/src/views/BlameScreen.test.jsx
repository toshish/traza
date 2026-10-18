// @vitest-environment jsdom
//
// The Blame screen, rendered from GET /v1/blame as a 0.28.0 server answered
// it over the imported Claude Code fixture (a sub-agent's MultiEdit) and the
// insight corpus (an Edit with no recorded prompt), and over three Edits of
// one line (`blame_unconfirmed.json`): one failed, one with no result
// recorded, one applied.

import React from 'react';
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, act, cleanup, fireEvent, screen } from '@testing-library/react';
import item from '../test/fixtures/insight/blame_item.json';
import test from '../test/fixtures/insight/blame_test.json';
import none from '../test/fixtures/insight/blame_none.json';
import unconfirmed from '../test/fixtures/insight/blame_unconfirmed.json';

vi.mock('../lib/api.js', () => ({
  api: {
    blame: vi.fn(async (params) => (params.path === 'src/item.rs' ? item : params.path === 'tests/test_parser.py' ? test
      : params.line === unconfirmed.line ? unconfirmed : none)),
  },
}));

const { BlameScreen } = await import('./BlameScreen.jsx');
const { api } = await import('../lib/api.js');

async function show(query = '') {
  const go = vi.fn();
  const view = render(<BlameScreen go={go} params={new URLSearchParams(query)} identity={null} pushToast={vi.fn()} />);
  await act(async () => { for (let i = 0; i < 4; i += 1) await Promise.resolve(); });
  return { view, go };
}

beforeEach(() => { api.blame.mockClear(); });
afterEach(() => { cleanup(); window.history.replaceState(null, '', '#'); });

describe('blame', () => {
  it('names the step that introduced the line: the session, the prompt, the model and the tool', async () => {
    const { go } = await show('path=src/item.rs&line=' + encodeURIComponent('pub price: u64,'));
    expect(api.blame).toHaveBeenCalledWith({ path: 'src/item.rs', line: 'pub price: u64,', project: undefined, limit: 20 }, expect.anything());
    const match = screen.getByTestId('blame-match').textContent;
    expect(match).toContain('introduced');
    expect(match).toContain('2026-01-05 10:00:25Z');
    expect(match).toContain('MultiEdit');
    expect(match).toContain('in sub-agent general-purpose');
    expect(match).toContain('Add a price field to Item in src/item.rs');
    expect(match).toContain('claude-haiku-4-5-20251001');
    expect(match).toContain('/tmp/demo-shop/src/item.rs');
    fireEvent.click(screen.getByText('Open the step on its tape'));
    expect(go).toHaveBeenCalledWith(['run', '11111111-1111-4111-8111-111111111111'], { tenant: '', step: '10' });
    expect(screen.getByTestId('blame-matches').textContent).toContain('from 14 edit calls checked in every project');
  });

  it('names the applied edit, and lists an edit with no result recorded after it as an attempt', async () => {
    await show('path=src/cart.rs&line=' + encodeURIComponent(unconfirmed.line));
    const cards = screen.getAllByTestId('blame-match');
    expect(cards.map((c) => c.dataset.outcome)).toEqual(['applied', 'unconfirmed']);
    expect(cards[0].textContent).toContain('introduced');
    expect(cards[0].textContent).toContain('session-applied');
    expect(cards[1].textContent).toContain('attempted');
    expect(cards[1].textContent).not.toContain('introduced');
    expect(cards[1].textContent).toContain('may not have happened');
    // The failed edit wrote nothing: it is not here at all.
    expect(screen.getByTestId('blame-matches').textContent).not.toContain('session-failed');
    expect(screen.getByTestId('blame-matches').textContent).toContain('1 introduction, then 1 attempt with no result recorded, from 3 edit calls checked');
  });

  it('says when no prompt or model was recorded, rather than inventing one', async () => {
    await show('path=tests/test_parser.py&line=' + encodeURIComponent("    assert parse('  Mixed ') == 'mixed'"));
    const match = screen.getByTestId('blame-match').textContent;
    expect(match).toContain('No prompt recorded on an enclosing turn.');
    expect(match).toContain('model that asked—');
  });

  it('says what blame cannot see when no recorded edit introduced the line', async () => {
    await show('path=src/cart.rs&line=nothing');
    const card = screen.getByTestId('blame-none').textContent;
    expect(card).toContain('No recorded edit introduced this line');
    expect(card).toContain('sed -i');
  });

  it('asks with the path, the exact line and any repository roots, all in the route', async () => {
    await show();
    expect(api.blame).not.toHaveBeenCalled();
    fireEvent.change(screen.getByLabelText('File path'), { target: { value: 'src/item.rs' } });
    fireEvent.change(screen.getByLabelText('Line text'), { target: { value: '  pub price: u64,' } });
    fireEvent.change(screen.getByLabelText('Repository roots'), { target: { value: '/tmp/demo-shop\n/srv/app/*\n' } });
    fireEvent.click(screen.getByText('Blame'));
    const params = new URLSearchParams(window.location.hash.split('?')[1]);
    expect(params.get('path')).toBe('src/item.rs');
    expect(params.get('line')).toBe('  pub price: u64,');
    expect(params.getAll('project')).toEqual(['/tmp/demo-shop', '/srv/app/*']);
  });

  it('takes a repository root with a space in it whole: one root per line', async () => {
    await show('path=src/item.rs&line=' + encodeURIComponent('pub price: u64,'));
    fireEvent.change(screen.getByLabelText('Repository roots'), { target: { value: '  /Users/me/My Repo  \n\n/srv/app' } });
    fireEvent.keyDown(screen.getByLabelText('Repository roots'), { key: 'Enter', metaKey: true });
    const params = new URLSearchParams(window.location.hash.split('?')[1]);
    expect(params.getAll('project')).toEqual(['/Users/me/My Repo', '/srv/app']);
    // Roots from the route come back one per line.
    cleanup();
    await show('path=src/item.rs&line=x&project=' + encodeURIComponent('/Users/me/My Repo') + '&project=%2Fsrv%2Fapp');
    expect(screen.getByLabelText('Repository roots').value).toBe('/Users/me/My Repo\n/srv/app');
    expect(api.blame).toHaveBeenLastCalledWith(expect.objectContaining({ project: ['/Users/me/My Repo', '/srv/app'] }), expect.anything());
  });
});
