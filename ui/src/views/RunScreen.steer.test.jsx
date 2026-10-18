// @vitest-environment jsdom
//
// The Run screen's v0.28 panels — the agent graph, the run's nudges, its
// forks, and Fork from here — rendered from what a real 0.28.0 server
// answered for simulated runs: a support agent's tape (tkt-4101), the
// topology of a support-app run whose agents ping-ponged until a policy
// stopped it, a run's delivered and expired nudges, and a fork listing.

import React from 'react';
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, act, cleanup, fireEvent, screen, within } from '@testing-library/react';
import tape from '../test/fixtures/tape_tkt4101.json';
import topology from '../test/fixtures/topology_pingpong.json';
import single from '../test/fixtures/topology_single.json';
import nudges from '../test/fixtures/nudges.json';
import forks from '../test/fixtures/forks.json';
import forkCreated from '../test/fixtures/fork_created.json';
import forkStatus from '../test/fixtures/fork_status.json';

vi.mock('../lib/api.js', () => ({
  api: {
    tape: vi.fn(async () => tape),
    fuseRun: vi.fn(async () => ({ verdict: 'continue' })),
    sessions: vi.fn(async () => ({ sessions: [] })),
    createReplay: vi.fn(async () => forkCreated),
    replay: vi.fn(async () => forkStatus),
    payload: vi.fn(async () => ''),
    topology: vi.fn(async () => topology),
    nudges: vi.fn(async () => nudges),
    nudge: vi.fn(async () => nudges.nudges[0]),
    forks: vi.fn(async () => forks),
    fateRun: vi.fn(async () => null),
  },
}));

const { RunScreen } = await import('./RunScreen.jsx');
const { api } = await import('../lib/api.js');

const RW = { auth: 'token', scope: 'rw', operator: true, tenant: null };
const RO = { auth: 'token', scope: 'ro', operator: false, tenant: 'acme' };

async function show({ identity = RW, params = new URLSearchParams('tenant=') } = {}) {
  const go = vi.fn();
  render(<RunScreen runId="tkt-4101" go={go} params={params} identity={identity} pushToast={vi.fn()} />);
  await act(async () => { for (let i = 0; i < 6; i += 1) await Promise.resolve(); });
  return { go };
}

beforeEach(() => {
  Object.values(api).forEach((fn) => fn.mockClear());
  api.topology.mockImplementation(async () => topology);
  Object.defineProperty(document, 'hidden', { configurable: true, get: () => false });
});
afterEach(cleanup);

describe('the agents', () => {
  it('draws the agents, the handoffs with counts, and the ping-pong pair', async () => {
    await show();
    const card = screen.getByTestId('topology-card');
    expect(within(card).getAllByTestId('topology-node')).toHaveLength(3);
    const edges = within(card).getAllByTestId('topology-edge');
    expect(edges.filter((e) => e.dataset.kind === 'handoff' && e.dataset.cycle === 'true')).toHaveLength(2);
    expect(edges.filter((e) => e.dataset.kind === 'delegation')).toHaveLength(2);
    expect(card.textContent).toMatch(/ping-pong\s*billing → triage → billing\s*×3/);
    // The turns, in step order.
    expect(card.textContent).toContain('turns, in step order');
    expect(within(card).getByLabelText('Turn sequence').textContent.replace(/›/g, ' ').split(/\s+/).filter(Boolean))
      .toEqual(topology.sequence);
  });

  it('says one agent in a sentence instead of drawing it', async () => {
    api.topology.mockImplementation(async () => single);
    await show();
    const card = screen.getByTestId('topology-card');
    expect(within(card).queryAllByTestId('topology-node')).toHaveLength(0);
    expect(card.textContent).toMatch(/One agent, handle_ticket, and\s+no handoffs/);
  });
});

describe('the nudges', () => {
  it('lists the run\'s nudges with how each ended, newest first', async () => {
    await show();
    const card = screen.getByTestId('nudges-card');
    const items = within(card).getAllByTestId('nudge');
    expect(items.map((n) => n.dataset.status)).toEqual(['expired', 'delivered']);
    expect(items[1].textContent).toContain('after 2.67 s, at the run\'s pre-call check');
    expect(items[1].textContent).toContain('The order search keeps coming back empty.');
  });

  it('offers the composer to a writer only', async () => {
    await show();
    expect(within(screen.getByTestId('nudges-card')).getByText('Nudge')).toBeTruthy();
    cleanup();
    await show({ identity: RO, params: new URLSearchParams() });
    expect(within(screen.getByTestId('nudges-card')).queryByText('Nudge')).toBeNull();
  });
});

describe('fork from here', () => {
  async function openFork(index) {
    const step = screen.getAllByTestId('tape-step')[index];
    fireEvent.click(step.querySelector('[role="row"]'));
    fireEvent.click(within(step).getByText('Fork from here'));
    return screen.getByRole('dialog', { name: `Fork tkt-4101 at step ${index}` });
  }

  it('opens on a tool step with its recorded result, and creates the fork with the edit and the live point', async () => {
    await show();
    const dialog = await openFork(1);
    const editor = within(dialog).getByLabelText('Edited tool result');
    expect(JSON.parse(editor.value)).toEqual(tape.steps[1].response);
    fireEvent.change(editor, { target: { value: '{"orders": [], "query": "E-9904", "note": "no match"}' } });
    await act(async () => { fireEvent.click(within(dialog).getByText('Create fork')); });
    expect(api.createReplay).toHaveBeenCalledWith({
      run: 'tkt-4101', mode: 'strict', tenant: '',
      edits: [{ step: 1, tool_result: { orders: [], query: 'E-9904', note: 'no match' } }], live_after: 1,
    });
    const created = within(dialog).getByTestId('fork-created').textContent;
    expect(created).toContain(`export TRAZA_FORK_ID=${forkCreated.fork_id}`);
    expect(created).toContain('export TRAZA_FORK_PARENT=tkt-4101');
    expect(created).toContain(`/v1/replays/${forkCreated.id}/openai/v1`);
    expect(created).toContain('traza.fork("tkt-4101", at_step=1, tool_result={"orders": [], "query": "E-9904", "note": "no match"}) as fork:');
    expect(created).toContain("traza fork tkt-4101 --at 1 --tool-result '{\"orders\":[],\"query\":\"E-9904\",\"note\":\"no match\"}' -- python agent.py");
    await act(async () => { fireEvent.click(within(dialog).getByText('What has it served?')); });
    expect(within(dialog).getByTestId('fork-status').textContent).toMatch(/went live at .*, 1 live calls · tool_result at 1 served/);
    // The listing reads again for the new fork.
    expect(api.forks).toHaveBeenCalledTimes(2);
  });

  it('edits a model answer as text, and rewinds without an edit', async () => {
    await show();
    let dialog = await openFork(4);
    const editor = within(dialog).getByLabelText('Edited answer');
    expect(editor.value).toBe(tape.steps[4].response[0].content);
    fireEvent.change(editor, { target: { value: 'I could not find that order; a person will check it.' } });
    fireEvent.click(within(dialog).getByText('Go live after this step'));
    await act(async () => { fireEvent.click(within(dialog).getByText('Create fork')); });
    expect(api.createReplay).toHaveBeenLastCalledWith({
      run: 'tkt-4101', mode: 'strict', tenant: '', edits: [{ step: 4, response: 'I could not find that order; a person will check it.' }],
    });
    fireEvent.click(within(dialog).getByText('Done'));

    dialog = await openFork(2);
    fireEvent.click(within(dialog).getByText('Rewind: go live at step 2'));
    fireEvent.click(within(dialog).getByText('loose'));
    await act(async () => { fireEvent.click(within(dialog).getByText('Create fork')); });
    expect(api.createReplay).toHaveBeenLastCalledWith({ run: 'tkt-4101', mode: 'loose', tenant: '', live_after: 1 });
  });

  it('refuses JSON that does not parse before asking the server', async () => {
    await show();
    const dialog = await openFork(1);
    fireEvent.change(within(dialog).getByLabelText('Edited tool result'), { target: { value: '{"orders": [' } });
    fireEvent.click(within(dialog).getByText('Create fork'));
    expect(within(dialog).getByRole('alert').textContent).toMatch(/That is not JSON/);
    expect(api.createReplay).not.toHaveBeenCalled();
  });

  it('shows the server\'s refusal, naming the step', async () => {
    api.createReplay.mockImplementationOnce(async () => {
      throw Object.assign(new Error('x'), {
        status: 400, what: 'step 1 is a tool call (tool:search_orders), so a "response" edit cannot replace it: a tool result is edited with "tool_result"',
      });
    });
    await show();
    const dialog = await openFork(1);
    await act(async () => { fireEvent.click(within(dialog).getByText('Create fork')); });
    expect(within(dialog).getByRole('alert').textContent).toMatch(/step 1 is a tool call/);
  });
});

describe('the forks', () => {
  it('lists each fork with its cut, outcome, cost against the parent, where it parted, and the diff', async () => {
    const { go } = await show();
    const card = screen.getByTestId('forks-card');
    const [fork] = within(card).getAllByTestId('fork-row');
    expect(fork.textContent).toContain('cut at step 1');
    expect(fork.textContent).toContain('failure');
    expect(fork.textContent).toContain('3 steps (−2)');
    expect(fork.textContent).toContain('parent success');
    expect(fork.textContent).toContain('step 1 of a and step 1 of b (tool:search_orders) received the same input and produced a different output');
    fireEvent.click(within(fork).getByText('diff against the parent'));
    expect(go).toHaveBeenCalledWith(['diff'], { a: 'tkt-4101', b: forks.forks[0].run, tenant: '' });
  });
});
