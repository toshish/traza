// @vitest-environment jsdom
//
// The trace's way into its run. A span's tenant is definite — `$tenant` is
// omitted for the default tenant — so the run opens in that tenant, named
// even when it is the default: unnamed, the tape route reads every tenant.

import React from 'react';
import { describe, it, expect, vi, afterEach } from 'vitest';
import { render, act, cleanup, fireEvent, screen } from '@testing-library/react';

const spansOf = (tenant) => [{
  trace_id: 't1', span_id: 'root', parent_span_id: null, name: 'agent', service: 'weather-agent',
  start_time_ns: 1_000_000_000, end_time_ns: 2_000_000_000, status: 'ok', events: [],
  attributes: { 'session.id': 'weather-7', 'gen_ai.request.model': 'gpt-4o' },
  ...(tenant ? { $tenant: tenant } : {}),
}];

vi.mock('../lib/api.js', () => ({
  api: { trace: vi.fn(async () => ({ trace_id: 't1', spans: spansOf(''), annotations: [] })) },
}));

const { TraceScreen } = await import('./TraceScreen.jsx');
const { api } = await import('../lib/api.js');

async function show() {
  const go = vi.fn();
  render(<TraceScreen traceId="t1" go={go} params={new URLSearchParams()} pushToast={vi.fn()} />);
  await act(async () => { for (let i = 0; i < 4; i += 1) await Promise.resolve(); });
  return go;
}

afterEach(cleanup);

describe("a trace's run", () => {
  it('opens in the default tenant, named, when the root span carries none', async () => {
    const go = await show();
    fireEvent.click(screen.getByText('Run'));
    expect(go).toHaveBeenCalledWith(['run', 'weather-7'], { tenant: '' });
    fireEvent.click(screen.getByText('Session'));
    expect(go).toHaveBeenCalledWith(['sessions', 'weather-7'], { tenant: '' });
  });

  it("opens in the root span's tenant", async () => {
    api.trace.mockResolvedValueOnce({ trace_id: 't1', spans: spansOf('acme'), annotations: [] });
    const go = await show();
    fireEvent.click(screen.getByText('Run'));
    expect(go).toHaveBeenCalledWith(['run', 'weather-7'], { tenant: 'acme' });
  });
});
