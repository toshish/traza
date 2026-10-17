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
