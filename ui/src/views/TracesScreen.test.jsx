// @vitest-environment jsdom
//
// The span-search screen, driven as a component.
//
// Both bugs this covers were invisible to a unit test of the helpers, because
// the screen had stopped calling the helpers. The token column carried its own
// copy of the semantic-convention precedence, and it had drifted away from
// src/semconv.rs; the two globals the toolbar reached for were shadowed by a
// local named `window`, so they read `undefined` through an optional chain and
// degraded in silence. Both fail here now.

import React from 'react';
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, act, cleanup, fireEvent, screen } from '@testing-library/react';

vi.mock('../lib/api.js', () => ({
  api: {
    spans: vi.fn(async () => ({ spans: [], cost: null, next_cursor: null })),
    series: vi.fn(async () => ({ buckets: [], bucket_ns: 1, since_ns: 0 })),
  },
}));

const { TracesScreen } = await import('./TracesScreen.jsx');
const { api } = await import('../lib/api.js');

const span = (id, attributes) => ({
  trace_id: `trace-${id}`,
  span_id: id,
  name: 'chat',
  service: 'agent',
  start_time_ns: 1_700_000_000_000_000_000,
  end_time_ns: 1_700_000_000_002_000_000,
  status: 'ok',
  attributes,
});

/** Renders the screen and waits for the initial reads to land. */
async function show(spans) {
  api.spans.mockResolvedValue({ spans, cost: null, next_cursor: null });
  const view = render(<TracesScreen go={() => {}} params={new URLSearchParams()} />);
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  });
  return view;
}

/** The cells of the one rendered span row, in column order. */
function cells() {
  const rows = Array.from(document.querySelectorAll('[role="row"]'));
  // The first `role="row"` is the header; the body row follows it.
  const body = rows[rows.length - 1];
  return Array.from(body.children).map((node) => node.textContent.trim());
}

beforeEach(() => {
  api.spans.mockClear();
  api.series.mockClear();
});

afterEach(cleanup);

describe('the token column', () => {
  // Column order is start, service, name, trace, bar, duration, tokens, cost,
  // status — so tokens is index 6 and cost index 7.
  const TOKENS = 6;
  const COST = 7;

  it('resolves the current OTel input/output names', async () => {
    // The names OTel actually specifies today, and the ones src/semconv.rs and
    // the trace detail already resolved. This is the case that rendered blank.
    await show([span('a', {
      'gen_ai.usage.input_tokens': 5138,
      'gen_ai.usage.output_tokens': 7488,
    })]);
    expect(cells()[TOKENS]).toBe('12,626');
  });

  it('still resolves the deprecated prompt/completion names', async () => {
    await show([span('b', {
      'gen_ai.usage.prompt_tokens': 5138,
      'gen_ai.usage.completion_tokens': 7488,
    })]);
    expect(cells()[TOKENS]).toBe('12,626');
  });

  it("resolves Traza's own llm.* shorthand", async () => {
    // `llm.prompt_tokens`, the key semconv.rs defines — not the
    // `llm.usage.prompt_tokens` the old copy invented, which nothing emits.
    await show([span('c', {
      'llm.prompt_tokens': 100,
      'llm.completion_tokens': 25,
    })]);
    expect(cells()[TOKENS]).toBe('125');
  });

  it('prefers an explicit total over the per-direction counts', async () => {
    await show([span('d', {
      'gen_ai.usage.input_tokens': 1,
      'gen_ai.usage.output_tokens': 1,
      'gen_ai.usage.total_tokens': 12_626,
    })]);
    expect(cells()[TOKENS]).toBe('12,626');
  });

  it('leaves both cells empty for a span carrying no LLM facts', async () => {
    await show([span('e', { 'research.stage': 'monitor' })]);
    expect(cells()[TOKENS]).toBe('');
    expect(cells()[COST]).toBe('');
  });

  it('renders a metered cost', async () => {
    await show([span('f', { 'llm.cost_usd': 0.0421 })]);
    expect(cells()[COST]).not.toBe('');
  });
});

describe('the toolbar globals', () => {
  it('copies a curl command with an absolute URL', async () => {
    // A local named `window` used to shadow the global here, so the origin
    // resolved to '' and the command curl received had no host in it.
    const written = [];
    Object.defineProperty(globalThis.navigator, 'clipboard', {
      configurable: true,
      value: { writeText: (text) => { written.push(text); return Promise.resolve(); } },
    });

    await show([]);
    fireEvent.click(screen.getByText('Copy as curl'));

    expect(written).toHaveLength(1);
    expect(written[0]).toContain(`'${globalThis.location.origin}/v1/spans`);
    expect(written[0]).not.toContain("'/v1/spans");
  });

  it('asks for a name when saving a view', async () => {
    // The same shadowing made `prompt` undefined, so every saved view silently
    // took the fallback name and the dialog never appeared.
    const asked = [];
    Object.defineProperty(globalThis, 'prompt', {
      configurable: true,
      value: (message) => { asked.push(message); return 'slow research calls'; },
    });

    await show([]);
    fireEvent.click(screen.getByText('+ save current'));

    expect(asked).toEqual(['Name this view']);
    expect(screen.getByText('slow research calls')).toBeTruthy();
  });
});
