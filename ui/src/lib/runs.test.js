import { describe, it, expect } from 'vitest';
import { pyLiteral, replayEnv, replayPython, routeTenant, splitFingerprint, tapeTotals, tenantParams, usageBars } from './runs.js';
import { rulerTicks } from '../components/charts/Marks.jsx';
import fuseRuns from '../test/fixtures/fuse_runs.json';
import tape from '../test/fixtures/tape_weather9.json';
import replay from '../test/fixtures/replay.json';

const stopped = fuseRuns.runs.find((run) => run.verdict === 'stop');

describe('tenantParams', () => {
  it('names a known tenant, the default one as an empty value', () => {
    expect(tenantParams('acme')).toEqual({ tenant: 'acme' });
    // `tenant=` is the default tenant on tapes, diffs and replays; naming
    // none there is the operator view across every tenant.
    expect(tenantParams('')).toEqual({ tenant: '' });
  });

  it('names nothing when the tenant is unknown', () => {
    expect(tenantParams(undefined)).toBeUndefined();
    expect(tenantParams(null)).toBeUndefined();
  });
});

describe('routeTenant', () => {
  it('tells an empty tenant parameter from an absent one', () => {
    expect(routeTenant(new URLSearchParams('tenant=acme'))).toBe('acme');
    expect(routeTenant(new URLSearchParams('tenant='))).toBe('');
    expect(routeTenant(new URLSearchParams('step=2'))).toBeUndefined();
  });
});

describe('usageBars', () => {
  it('draws one bar per limit in force, against the verdict document', () => {
    const bars = usageBars(stopped);
    expect(bars.map((b) => b.key)).toEqual(['max_cost_usd', 'max_steps', 'max_repeats', 'max_error_streak']);
    const repeats = bars.find((b) => b.key === 'max_repeats');
    expect(repeats).toMatchObject({ used: 4, max: 4, level: 'reached', text: '4 / 4' });
    expect(bars.find((b) => b.key === 'max_cost_usd').text).toBe('$0.420 / $2.00');
  });

  it('marks 80% as near, which is where the guard starts to warn', () => {
    const warn = fuseRuns.runs.find((run) => run.verdict === 'warn');
    expect(usageBars(warn).find((b) => b.key === 'max_cost_usd')).toMatchObject({ level: 'near' });
  });

  it('draws nothing for an unbounded run', () => {
    expect(usageBars({ limits: {}, usage: { steps: 3 } })).toEqual([]);
  });
});

describe('splitFingerprint', () => {
  it('cuts the hash by shape, so a # in a name survives', () => {
    expect(splitFingerprint('tool:search_orders#be2c24774ede8979'))
      .toEqual({ kind: 'tool', name: 'search_orders', hash: 'be2c24774ede8979' });
    expect(splitFingerprint('tool:issue#42#0123456789abcdef'))
      .toEqual({ kind: 'tool', name: 'issue#42', hash: '0123456789abcdef' });
    expect(splitFingerprint('llm:gpt-4o')).toEqual({ kind: 'llm', name: 'gpt-4o', hash: '' });
  });
});

describe('replay snippets', () => {
  it('points both SDKs at the replay under the public base, never printing a token', () => {
    const lines = replayEnv(replay, 'https://host/x/org/project', 'token').split('\n');
    expect(lines).toEqual([
      `export OPENAI_BASE_URL=https://host/x/org/project${replay.base_urls.openai}`,
      'export OPENAI_API_KEY=$TRAZA_TOKEN',
      `export ANTHROPIC_BASE_URL=https://host/x/org/project${replay.base_urls.anthropic}`,
      'export ANTHROPIC_API_KEY=$TRAZA_TOKEN',
    ]);
    expect(replayEnv(replay, 'http://x', 'open')).toMatch(/OPENAI_API_KEY=unused/);
  });

  it("is six lines of Python sending step 0's recorded messages", () => {
    const lines = replayPython(tape).split('\n');
    expect(lines).toHaveLength(6);
    expect(lines[2]).toBe('messages = [{"content": "You are a weather assistant.", "role": "system"}, '
      + '{"content": "What\'s the weather in Paris?", "role": "user"}]  # step 0 as recorded');
    expect(lines[3]).toContain('model="gpt-4o-2024-08-06"');
  });

  it('writes Python literals for JSON values', () => {
    expect(pyLiteral({ a: true, b: null, c: [1, false, 'x'] })).toBe('{"a": True, "b": None, "c": [1, False, "x"]}');
  });
});

describe('tapeTotals', () => {
  it("adds up what the steps reported, and says how many carried a cost", () => {
    const totals = tapeTotals(tape);
    expect(totals.steps).toBe(4);
    expect(totals.tokens).toBe(82 + 17 + 120 + 14);
    expect(totals.priced).toBe(2);
    expect(totals.cost).toBeCloseTo(0.00082, 8);
  });
});

describe('rulerTicks', () => {
  it('keeps every label clear of its neighbours', () => {
    expect(rulerTicks(0, 6)).toBe(6);
    expect(rulerTicks(265, 6)).toBe(2);
    expect(rulerTicks(400, 6)).toBe(4);
    expect(rulerTicks(2000, 6)).toBe(6);
    expect(rulerTicks(40, 6)).toBe(1);
  });
});
