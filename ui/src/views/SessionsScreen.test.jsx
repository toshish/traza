// @vitest-environment jsdom
//
// Sessions and the run each one opens. Session identity is
// `(tenant, session_id)`, and on the tape route naming no tenant is the
// operator view across every tenant, so every link from here names the
// session's tenant, the default one as an empty value.

import React from 'react';
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, act, cleanup, fireEvent, screen } from '@testing-library/react';

vi.mock('../lib/api.js', () => ({
  api: {
    // A row omits `tenant` for the default tenant, as the server does.
    sessions: vi.fn(async () => ({
      sessions: [
        { session_id: 'weather-7', trace_count: 1, span_count: 3, llm_calls: 2, total_tokens: 233, cost_usd: 0, error_count: 0, first_start_ns: 1, last_end_ns: 2 },
        { session_id: 'weather-7', tenant: 'acme', trace_count: 1, span_count: 3, llm_calls: 2, total_tokens: 233, cost_usd: 0, error_count: 0, first_start_ns: 1, last_end_ns: 2 },
      ],
    })),
    session: vi.fn(async () => ({
      session_id: 'weather-7', trace_count: 1, span_count: 3, llm_calls: 2, total_tokens: 233, cost_usd: 0,
      error_count: 0, first_start_ns: 1, last_end_ns: 2, traces: [],
    })),
  },
}));

const { SessionsScreen, SessionScreen } = await import('./SessionsScreen.jsx');
const { api } = await import('../lib/api.js');

async function flush() {
  await act(async () => { for (let i = 0; i < 4; i += 1) await Promise.resolve(); });
}

beforeEach(() => Object.values(api).forEach((fn) => fn.mockClear()));
afterEach(cleanup);

describe('the session list', () => {
  it("opens each session in its own tenant, naming the default one", async () => {
    const go = vi.fn();
    render(<SessionsScreen go={go} params={new URLSearchParams()} />);
    await flush();
    const rows = screen.getAllByRole('link').filter((node) => node.textContent.startsWith('weather-7'));
    expect(rows).toHaveLength(2);
    fireEvent.click(rows[0]);
    expect(go).toHaveBeenLastCalledWith(['sessions', 'weather-7'], { tenant: '' });
    fireEvent.click(rows[1]);
    expect(go).toHaveBeenLastCalledWith(['sessions', 'weather-7'], { tenant: 'acme' });
  });
});

describe('a session nothing priced', () => {
  // A coding-agent transcript records tokens but no cost, and with no
  // pricing table nothing derives one: cost reads "—", and so must cost per
  // turn — `0.00000` would say the session was free.
  it('shows no cost per turn in the list or on the session', async () => {
    render(<SessionsScreen go={vi.fn()} params={new URLSearchParams()} />);
    await flush();
    const row = screen.getAllByRole('link').find((node) => node.textContent.startsWith('weather-7'));
    expect(row.textContent).not.toContain('0.00000');
    cleanup();
    render(<SessionScreen sessionId="weather-7" go={vi.fn()} params={new URLSearchParams('tenant=')} />);
    await flush();
    const card = screen.getByText('cost / turn').parentElement;
    expect(card.textContent).toBe('cost / turn—');
  });

  it('still divides a priced cost by the turns', async () => {
    api.session.mockResolvedValueOnce({
      session_id: 'paid', trace_count: 4, span_count: 8, llm_calls: 4, total_tokens: 900,
      cost_usd: 0.02, cost_metered_calls: 4, error_count: 0, first_start_ns: 1, last_end_ns: 2, traces: [],
    });
    render(<SessionScreen sessionId="paid" go={vi.fn()} params={new URLSearchParams('tenant=')} />);
    await flush();
    expect(screen.getByText('cost / turn').parentElement.textContent).toBe('cost / turn0.00500');
  });
});

describe('what a turn is', () => {
  // A recorder marks one turn span per user prompt, and the server counts
  // those as `turn_count` (`turns_from: "prompts"`). An imported coding-agent
  // session is one trace however many prompts it held, so reading traces as
  // turns showed every such session as 1 turn and divided its whole cost by
  // one. A producer that marks no prompts gets traces, and says so.
  const prompted = {
    session_id: 'import-1', trace_count: 1, turn_count: 8, turns_from: 'prompts', span_count: 40, llm_calls: 20,
    total_tokens: 9000, cost_usd: 0.08, cost_metered_calls: 20, error_count: 0, first_start_ns: 1, last_end_ns: 3,
  };
  const traced = {
    session_id: 'otel-2', trace_count: 4, turn_count: 4, turns_from: 'traces', span_count: 12, llm_calls: 4,
    total_tokens: 800, cost_usd: 0.06, cost_metered_calls: 4, error_count: 0, first_start_ns: 1, last_end_ns: 2,
  };
  const rowOf = (id) => screen.getAllByRole('link').find((node) => node.textContent.startsWith(id));
  const order = () => screen.getAllByRole('link').map((node) => ['import-1', 'otel-2'].find((id) => node.textContent.startsWith(id)));

  it('counts the prompts as turns, divides cost by them, and says what was counted', async () => {
    api.sessions.mockResolvedValueOnce({ sessions: [prompted, traced] });
    render(<SessionsScreen go={vi.fn()} params={new URLSearchParams()} />);
    await flush();
    const turns = rowOf('import-1').children[1];
    expect(turns.textContent).toBe('8');
    expect(turns.getAttribute('title')).toMatch(/user prompts/);
    expect(rowOf('import-1').children[6].textContent).toBe('0.01000');
    const other = rowOf('otel-2').children[1];
    expect(other.textContent).toBe('4');
    expect(other.getAttribute('title')).toMatch(/traces — this producer marks no prompts, so each trace counts as a turn/);
    expect(rowOf('otel-2').children[6].textContent).toBe('0.01500');
  });

  it('sorts the longest and the worst cost per turn by turns, not traces', async () => {
    api.sessions.mockResolvedValueOnce({ sessions: [traced, prompted] });
    render(<SessionsScreen go={vi.fn()} params={new URLSearchParams()} />);
    await flush();
    fireEvent.click(screen.getByText('longest'));
    expect(order()).toEqual(['import-1', 'otel-2']);
    fireEvent.click(screen.getByText('worst cost/turn'));
    expect(order()).toEqual(['otel-2', 'import-1']);
  });

  it('falls back to traces from a server that sends no turn count', async () => {
    const { turn_count: _count, turns_from: _from, ...older } = traced;
    api.sessions.mockResolvedValueOnce({ sessions: [{ ...older, cost_usd: 0.04 }] });
    render(<SessionsScreen go={vi.fn()} params={new URLSearchParams()} />);
    await flush();
    const row = rowOf('otel-2');
    expect(row.children[1].textContent).toBe('4');
    expect(row.children[1].getAttribute('title')).toMatch(/traces/);
    expect(row.children[6].textContent).toBe('0.01000');
  });

  it('labels the session and its per-trace rows by what was counted', async () => {
    api.session.mockResolvedValueOnce({
      ...prompted, traces: [{ trace_id: 'a'.repeat(32), root_name: 'session', span_count: 40, total_tokens: 9000, cost_usd: 0.08, error_count: 0, first_start_ns: 1, last_end_ns: 3 }],
    });
    render(<SessionScreen sessionId="import-1" go={vi.fn()} params={new URLSearchParams('tenant=')} />);
    await flush();
    const card = screen.getByText('turns').parentElement;
    expect(card.textContent).toBe('turns8');
    expect(card.getAttribute('title')).toMatch(/user prompts/);
    expect(screen.getByText('cost / turn').parentElement.textContent).toBe('cost / turn0.01000');
    // The rows are traces, not turns, when the turns were prompts.
    expect(screen.getByRole('columnheader', { name: 'trace' })).toBeTruthy();
    expect(screen.queryByRole('columnheader', { name: 'turn' })).toBeNull();
    cleanup();
    api.session.mockResolvedValueOnce({ ...traced, traces: [] });
    render(<SessionScreen sessionId="otel-2" go={vi.fn()} params={new URLSearchParams('tenant=')} />);
    await flush();
    expect(screen.getByText('turns').parentElement.getAttribute('title')).toMatch(/each trace counts as a turn/);
    expect(screen.getByRole('columnheader', { name: 'turn' })).toBeTruthy();
  });
});

describe('one session', () => {
  it('reads the session in the tenant its route names, and opens the run there', async () => {
    const go = vi.fn();
    render(<SessionScreen sessionId="weather-7" go={go} params={new URLSearchParams('tenant=')} />);
    await flush();
    expect(api.session).toHaveBeenCalledWith('weather-7', { tenant: '' }, expect.anything());
    fireEvent.click(screen.getByText('Run'));
    expect(go).toHaveBeenCalledWith(['run', 'weather-7'], { tenant: '' });
  });

  it('opens the run in the tenant its spans agree on when the route names none', async () => {
    api.session.mockResolvedValueOnce({
      session_id: 'chat', tenant: 'acme', trace_count: 1, span_count: 1, llm_calls: 1, total_tokens: 1,
      cost_usd: 0, error_count: 0, first_start_ns: 1, last_end_ns: 2, traces: [],
    });
    const go = vi.fn();
    render(<SessionScreen sessionId="chat" go={go} params={new URLSearchParams()} />);
    await flush();
    expect(api.session).toHaveBeenCalledWith('chat', undefined, expect.anything());
    fireEvent.click(screen.getByText('Run'));
    expect(go).toHaveBeenCalledWith(['run', 'chat'], { tenant: 'acme' });
  });

  it('names no tenant when an unscoped answer cannot tell the default tenant from a mix', async () => {
    // Read without a tenant, an empty `tenant` is the default tenant OR
    // spans of several tenants; the run's scope is unknown, and the Run
    // screen says so rather than guessing.
    const go = vi.fn();
    render(<SessionScreen sessionId="weather-7" go={go} params={new URLSearchParams()} />);
    await flush();
    fireEvent.click(screen.getByText('Run'));
    expect(go).toHaveBeenCalledWith(['run', 'weather-7'], undefined);
  });
});
