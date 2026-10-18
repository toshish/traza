// @vitest-environment jsdom
//
// The Fate Map drawn from a synthetic GET /v1/fate/map answer in which two
// sibling branches read the same: the map's tokens are names plus a repeat
// bucket, so a tool literally named `lookup×2` called once and `lookup`
// called twice both read `tool:lookup×2`. Each is still its own branch.

import React from 'react';
import { describe, it, expect, vi, afterEach } from 'vitest';
import { render, cleanup, fireEvent } from '@testing-library/react';
import { FateMap } from './FateMap.jsx';

const stats = (visits, success, run, extra = {}) => ({
  depth: 0, visits, success, failure: visits - success, abandoned: 0, ended: 0,
  success_rate: success / visits, ci95: [0.1, 0.9], p_fail_given_here: (visits - success) / visits, lift: 1,
  entropy: 0, hotspot: false, fatal_fork: false, cost_usd_sum: 0, duration_ns_sum: 0,
  example_runs: [{ run, outcome: success ? 'success' : 'failure' }], children: [], ...extra,
});

const MAP = {
  agent: 'twin-agent', base_rate: 0.5, runs: 4, nodes_shown: 4,
  root: stats(4, 2, 'run-a', {
    token: '^',
    children: [stats(4, 2, 'run-a', {
      token: 'llm:model-x', depth: 1, entropy: 1,
      children: [
        stats(3, 2, 'run-a', { token: 'tool:lookup×2', depth: 2, ended: 3 }),
        stats(1, 0, 'run-b', { token: 'tool:lookup×2', depth: 2, ended: 1 }),
      ],
    })],
  }),
};

function Harness({ onSelect }) {
  const [selected, setSelected] = React.useState(null);
  return <FateMap map={MAP} selected={selected} collapsed={new Set()}
    onSelect={(key) => { setSelected(key); onSelect(key); }} />;
}

afterEach(() => { cleanup(); vi.restoreAllMocks(); });

describe('two sibling branches that read the same', () => {
  it('are drawn under keys of their own, and clicking the second selects the second', () => {
    const errors = vi.spyOn(console, 'error').mockImplementation(() => {});
    const onSelect = vi.fn();
    const { container } = render(<Harness onSelect={onSelect} />);
    const twins = [...container.querySelectorAll('[data-testid="fate-node"][data-token="tool:lookup×2"]')];
    expect(twins).toHaveLength(2);
    expect(twins[0].getAttribute('data-key')).not.toBe(twins[1].getAttribute('data-key'));
    expect(twins[0].getAttribute('aria-label')).toMatch(/^tool:lookup×2: 3 runs/);
    expect(twins[1].getAttribute('aria-label')).toMatch(/^tool:lookup×2: 1 runs/);

    fireEvent.click(twins[1]);
    expect(onSelect).toHaveBeenLastCalledWith(twins[1].getAttribute('data-key'));
    const drawn = [...container.querySelectorAll('[data-testid="fate-node"][data-token="tool:lookup×2"]')];
    expect(drawn.map((node) => node.getAttribute('aria-pressed'))).toEqual(['false', 'true']);
    expect(drawn[1].getAttribute('aria-label')).toMatch(/1 runs, success 0\.0%/);

    fireEvent.click(drawn[0]);
    expect([...container.querySelectorAll('[data-testid="fate-node"][data-token="tool:lookup×2"]')]
      .map((node) => node.getAttribute('aria-pressed'))).toEqual(['true', 'false']);

    const duplicate = errors.mock.calls.filter((args) => args.some((arg) => typeof arg === 'string' && /same key/i.test(arg)));
    expect(duplicate).toEqual([]);
  });
});
