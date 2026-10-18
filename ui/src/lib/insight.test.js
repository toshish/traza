// The insight screens' arithmetic, against answers a 0.28.0 server gave over
// the behaviour corpus (`seed --behaviours`), the imported Claude Code
// fixture and the insight corpus (src/test/fixtures/insight/CAPTURED.md).

import { describe, it, expect } from 'vitest';
import {
  AT_OR_ABOVE, RAMP_STEPS, bestSkill, branchWidth, cacheSegments, caretLine, ciContains, classTotals, compactPath,
  deltaDirection, DELTA_METRICS, fmtCi, fmtP, fmtPct, layoutFate, matchRoute, nodeAt, nodeMeans, pathKey, pathToTql,
  provenanceGroups, rateColor, runOnMap, shortName, skillReading, splitToken, stableWithoutRead, windowFrom, windowSince,
} from './insight.js';
import supportMap from '../test/fixtures/insight/fate_map_support.json';
import doomed from '../test/fixtures/insight/fate_run_doomed.json';
import healthy from '../test/fixtures/insight/fate_run_healthy.json';
import novel from '../test/fixtures/insight/fate_run_novel.json';
import cc from '../test/fixtures/insight/fate_run_cc.json';
import xray30 from '../test/fixtures/insight/xray_fix7_30.json';
import autopsy from '../test/fixtures/insight/cache_fix7.json';
import ccAutopsy from '../test/fixtures/insight/cache_cc1.json';
import tamper from '../test/fixtures/insight/tql_tamper.json';
import changes from '../test/fixtures/insight/changes.json';

describe('windows', () => {
  it('sends no since for the default window, which every route defaults to and the server caches', () => {
    expect(windowSince('30d')).toBeUndefined();
    expect(windowSince('nonsense')).toBeUndefined();
  });

  it('floors since to the hour, exactly, in integer nanoseconds', () => {
    const now = Date.UTC(2026, 9, 18, 10, 41, 7, 123);
    const since = windowSince('7d', now);
    expect(since).toBe(String(BigInt(Date.UTC(2026, 9, 11, 10, 0, 0)) * 1000000n));
    expect(since).toMatch(/^\d{19}$/);
  });

  it('reads the window from the route, falling back to the default', () => {
    expect(windowFrom(new URLSearchParams('window=1y'))).toBe('1y');
    expect(windowFrom(new URLSearchParams('window=2y'))).toBe('30d');
  });
});

describe('rates', () => {
  it('formats a rate, a Wilson interval and a p-value at the precision a reader can use', () => {
    expect(fmtPct(0.5444)).toBe('54.4%');
    expect(fmtPct(null)).toBe('—');
    expect(fmtCi([0.0435, 0.2136])).toBe('4.3–21.4%');
    expect(fmtP(2.2114329254814858e-14)).toBe('<0.001');
    expect(fmtP(0.008)).toBe('0.008');
    expect(fmtP(0.27)).toBe('0.27');
  });

  it('colours a branch ink at or above the base rate and down the measured ramp below it', () => {
    const base = supportMap.base_rate;
    expect(rateColor(0.9, base)).toBe(AT_OR_ABOVE);
    expect(rateColor(base, base)).toBe(AT_OR_ABOVE);
    expect(rateColor(0.5, base)).toBe(RAMP_STEPS[0].color); // 4.4 points below
    expect(rateColor(0.1, base)).toBe(RAMP_STEPS[3].color); // the search loop, 44 points below
    expect(ciContains([0.3315, 0.6685], base)).toBe(true);
    expect(ciContains([0.0435, 0.2136], base)).toBe(false);
  });
});

describe('the Fate Map', () => {
  it('lays the tree out left to right: one row per leaf, parents between their children', () => {
    const layout = layoutFate(supportMap.root, { colWidth: 100, padX: 0, padY: 0 });
    // `nodes_shown` counts the root.
    expect(layout.nodes).toHaveLength(supportMap.nodes_shown);
    expect(layout.edges).toHaveLength(supportMap.nodes_shown - 1);
    for (const entry of layout.nodes) {
      expect(entry.x).toBe(entry.depth * 100);
      const children = layout.edges.filter((e) => e.from === entry).map((e) => e.to);
      if (children.length) {
        expect(entry.y).toBe((children[0].y + children[children.length - 1].y) / 2);
        // The server orders siblings most visited first; the layout keeps it.
        expect(children.map((c) => c.node.visits)).toEqual([...children.map((c) => c.node.visits)].sort((a, b) => b - a));
      }
    }
    const leaves = layout.nodes.filter((n) => !(n.node.children || []).length).map((n) => n.y);
    expect(leaves).toEqual([...leaves].sort((a, b) => a - b));
  });

  it('draws a branch as wide as its runs, linearly, with a floor', () => {
    expect(branchWidth(180, 180)).toBe(18);
    expect(branchWidth(90, 180)).toBe(9);
    expect(branchWidth(1, 180)).toBe(1.5);
    const layout = layoutFate(supportMap.root);
    const loop = layout.byKey.get(pathKey(['llm:gpt-4o', 'tool:search_orders×3-5']));
    expect(loop.stroke).toBeCloseTo((50 / 180) * 18);
  });

  it('folds a collapsed branch and says how many children it hides', () => {
    const key = pathKey(['llm:gpt-4o']);
    const layout = layoutFate(supportMap.root, { collapsed: new Set([key]) });
    expect(layout.nodes).toHaveLength(2);
    expect(layout.byKey.get(key).hiddenChildren).toBe(3);
  });

  it('finds a node by path, and its means from the sums the map carries', () => {
    const loop = nodeAt(supportMap.root, ['llm:gpt-4o', 'tool:search_orders×3-5']);
    expect(loop.visits).toBe(50);
    expect(loop.fatal_fork).toBe(true);
    expect(nodeAt(supportMap.root, ['llm:gpt-4o', 'tool:nope'])).toBeNull();
    const means = nodeMeans(loop);
    expect(means.cost).toBeCloseTo(loop.cost_usd_sum / 50);
    expect(means.durationNs).toBeCloseTo(loop.duration_ns_sum / 50);
  });

  it('lights a live run up to its open last step, pooled with the bucket it may still grow into', () => {
    const lit = runOnMap(supportMap.root, healthy);
    const refund = ['llm:gpt-4o', 'tool:search_orders', 'llm:gpt-4o', 'tool:issue_refund'];
    expect(lit.atKey).toBe(pathKey(refund));
    expect(lit.keys.has(pathKey(refund))).toBe(true);
    expect([...lit.pooled].sort()).toEqual([pathKey(refund), pathKey([...refund.slice(0, 3), 'tool:issue_refund×2'])].sort());
    expect(lit.recommendedKey).toBe(pathKey([...refund, 'llm:gpt-4o']));
    expect(lit.unmatched).toEqual([]);
  });

  it('marks where a novel run left the known paths', () => {
    const lit = runOnMap(supportMap.root, novel);
    expect(lit.atKey).toBe(pathKey(['llm:gpt-4o']));
    expect(lit.unmatched).toEqual(['tool:delete_database']);
    expect(lit.divergence.took).toBe('tool:delete_database');
    expect(lit.recommendedKey).toBe(pathKey(['llm:gpt-4o', 'tool:search_orders']));
    expect(runOnMap(supportMap.root, doomed).atKey).toBe(pathKey(['llm:gpt-4o', 'tool:search_orders×3-5']));
  });

  it('turns a path into the TQL that finds the runs which took it', () => {
    expect(pathToTql(['llm:gpt-4o', 'tool:search_orders×3-5'])).toBe('^llm("gpt-4o") -> tool("search_orders"){3,5}');
    expect(pathToTql(['llm:gpt-4o', 'tool:issue_refund×2', 'agent:x×6+'])).toBe('^llm("gpt-4o") -> tool("issue_refund"){2} -> agent("x"){6,}');
    expect(pathToTql(['llm:a-very-long-cut-name…'])).toBeNull();
    expect(pathToTql([])).toBeNull();
    expect(pathToTql(cc.path)).toMatch(/^\^llm\("claude-sonnet-4-5-20250929"\) -> tool\("Read"\)/);
  });

  it('splits and shortens tokens without losing the repeat bucket', () => {
    expect(splitToken('llm:claude-sonnet-4-5')).toEqual({ kind: 'llm', name: 'claude-sonnet-4-5' });
    expect(splitToken('^')).toEqual({ kind: '', name: '^' });
    expect(shortName('search_orders_for_the_customer×3-5', 16)).toBe('search_orde…×3-5');
    expect(shortName('short', 16)).toBe('short');
  });

  it('cuts a long path around its focus, saying how many tokens each gap holds', () => {
    const items = compactPath(cc.path, 1, 7);
    expect(items[0]).toEqual({ token: cc.path[0], index: 0 });
    expect(items[items.length - 1]).toEqual({ token: cc.path[cc.path.length - 1], index: cc.path.length - 1 });
    const shown = items.filter((i) => !i.gap).length;
    const hidden = items.filter((i) => i.gap).reduce((n, i) => n + i.gap, 0);
    expect(shown + hidden).toBe(cc.path.length);
    expect(compactPath(['a', 'b'], 0, 7)).toHaveLength(2);
  });

  it("reads the map's own score honestly", () => {
    const best = bestSkill(supportMap.calibration);
    expect(best.depth).toBe(3);
    expect(best.skill).toBeCloseTo(0.3128);
    expect(skillReading(best.skill)).toBe('much better than the base rate');
    expect(skillReading(0.01)).toBe('no better than the base rate');
    expect(bestSkill({ depths: [] })).toBeNull();
  });
});

describe('the X-Ray', () => {
  it('totals a context by the step that introduced it, most tokens first', () => {
    const groups = provenanceGroups(xray30.chunks);
    expect(groups[0]).toMatchObject({ key: 's7', step: 7, kind: 'tool', name: 'Read', stale: 1 });
    expect(groups.reduce((sum, g) => sum + g.share, 0)).toBeCloseTo(1);
    expect(groups.reduce((sum, g) => sum + g.chunks, 0)).toBe(xray30.chunks.length);
  });

  it('totals tokens by colour class, matching the estimate', () => {
    const totals = classTotals(xray30.chunks);
    expect(totals.reduce((sum, c) => sum + c.tokens, 0)).toBe(xray30.totals.est_tokens);
    expect(totals.find((c) => c.id === 'definitions').tokens).toBe(154);
  });

  it("keeps an autopsy bar in one unit: the calibrated loss is taken back to the estimate's scale", () => {
    const expired = autopsy.steps.find((s) => s.cause === 'ttl_expired');
    const seg = cacheSegments(expired);
    expect(expired.est_tokens_lost).toBeGreaterThan(expired.est_tokens);
    expect(seg.lost).toBe(Math.min(expired.est_tokens, Math.round(expired.est_tokens_lost / expired.calibration)));
    expect(seg.served).toBe(0);
    const stable = autopsy.steps.find((s) => s.cause === 'stable');
    const s2 = cacheSegments(stable);
    expect(s2.served).toBe(stable.shared_est_tokens);
    expect(s2.served + s2.lost + s2.fresh).toBe(stable.est_tokens);
  });

  it('marks a held prefix the provider served nothing from', () => {
    const flagged = ccAutopsy.steps.filter(stableWithoutRead).map((s) => s.step_index);
    expect(flagged).toEqual([13]);
    expect(ccAutopsy.summary.disagreements).toBe(1);
  });
});

describe('changes and TQL', () => {
  it('marks which way a delta moved, by which way is worse', () => {
    const regression = changes.agents[0].changes[0];
    const error = DELTA_METRICS.find((m) => m.key === 'error_rate');
    const success = DELTA_METRICS.find((m) => m.key === 'success_rate');
    expect(deltaDirection(error, regression.deltas.error_rate)).toBe('worse');
    expect(deltaDirection(success, regression.deltas.success_rate)).toBe('worse');
    expect(deltaDirection(error, { before: 0.1, after: 0.1 })).toBe('none');
  });

  it('puts a caret under the column a refusal names, counting characters', () => {
    expect(caretLine('tool(bash)[stauts=error]', 12)).toBe(' '.repeat(11) + '^');
    expect(caretLine('llm(é)[x=1]', 8)).toBe(' '.repeat(7) + '^');
    expect(caretLine('q', 0)).toBe('');
  });

  it("routes a match to its run's tape at the first matched step, in its tenant", () => {
    expect(matchRoute(tamper.matches[0])).toEqual({ parts: ['run', 'fix-parser-7'], params: { step: '5', tenant: '' } });
  });
});
