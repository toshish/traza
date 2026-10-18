// The insight screens' arithmetic, against answers a 0.28.0 server gave over
// the behaviour corpus (`seed --behaviours`), the imported Claude Code
// fixture and the insight corpus (src/test/fixtures/insight/CAPTURED.md).

import { describe, it, expect } from 'vitest';
import {
  AT_OR_ABOVE, CACHE_CAUSE, RAMP_STEPS, bestSkill, branchWidth, cacheSegments, caretLine, ciContains, classTotals, compactPath,
  deltaDirection, DELTA_METRICS, detectorRate, entryKey, fmtCi, fmtP, fmtPct, keyTokens, layoutFate, matchRoute, nodeAt, nodeAtKey,
  nodeMeans, pathKey, pathToTql,
  provenanceGroups, rateColor, runOnMap, shortName, skillAt, skillReading, splitToken, stableWithoutRead, wilson, windowFrom, windowSince,
} from './insight.js';
import supportMap from '../test/fixtures/insight/fate_map_support.json';
import supportDepth2 from '../test/fixtures/insight/fate_map_support_depth2.json';
import supportMin100 from '../test/fixtures/insight/fate_map_support_min100.json';
import researcherMap from '../test/fixtures/insight/fate_map_researcher.json';
import findings from '../test/fixtures/insight/findings.json';
import doomed from '../test/fixtures/insight/fate_run_doomed.json';
import healthy from '../test/fixtures/insight/fate_run_healthy.json';
import novel from '../test/fixtures/insight/fate_run_novel.json';
import cc from '../test/fixtures/insight/fate_run_cc.json';
import xray30 from '../test/fixtures/insight/xray_fix7_30.json';
import autopsy from '../test/fixtures/insight/cache_fix7.json';
import ccAutopsy from '../test/fixtures/insight/cache_cc1.json';
import tamper from '../test/fixtures/insight/tql_tamper.json';
import changes from '../test/fixtures/insight/changes.json';
// A store of its own (CAPTURED.md): loop-agent called `search` once to
// seven times after one model call, and `Search` three times; mini-agent
// called a tool literally named `search×2` once, and `search` twice. The
// maps at min_support 1, and GET /v1/trajectories/search's answer, with
// the map's agent, to the query every node's "Find these runs" sends —
// and to the one the wave-3 dashboard sent.
import loopsMap from '../test/fixtures/insight/fate_map_loops.json';
import miniMap from '../test/fixtures/insight/fate_map_mini.json';
import loopAnswers from '../test/fixtures/insight/tql_loops.json';

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

  it('keys two branches that read the same apart, and finds each by its key', () => {
    // mini-agent: `search×2` called once beside `search` called twice — one
    // text, two branches, and one React key and one selection between them.
    const layout = layoutFate(miniMap.root);
    const twins = layout.nodes.filter((entry) => entry.depth === 2);
    expect(twins.map((entry) => entry.node.token)).toEqual(['tool:search×2', 'tool:search×2']);
    expect(new Set(layout.nodes.map((entry) => entry.key)).size).toBe(layout.nodes.length);
    expect(layout.edges.map((edge) => edge.to.key).sort()).toEqual(layout.nodes.slice(1).map((entry) => entry.key).sort());
    // The first keeps its plain path, so a link, a fork or a run's path that
    // names it by its text still opens it.
    expect(twins[0].key).toBe(pathKey(['llm:gpt-4o-mini', 'tool:search×2']));
    expect(nodeAtKey(miniMap.root, twins[0].key)).toBe(twins[0].node);
    expect(nodeAtKey(miniMap.root, twins[1].key)).toBe(twins[1].node);
    expect(nodeAtKey(miniMap.root, twins[1].key).example_runs.map((e) => e.run)).toEqual(['two']);
    const deep = layout.nodes.filter((entry) => entry.depth === 3);
    expect(nodeAtKey(miniMap.root, deep[1].key).example_runs.map((e) => e.run)).toEqual(['two']);
    // Folding the second folds only the second.
    const folded = layoutFate(miniMap.root, { collapsed: new Set([twins[1].key]) });
    expect(folded.nodes).toHaveLength(layout.nodes.length - 1);
    expect(folded.byKey.get(twins[0].key).hiddenChildren).toBe(0);
    // A key reads back as the tokens of its path, for display and search.
    expect(keyTokens(twins[1].key)).toEqual(['llm:gpt-4o-mini', 'tool:search×2']);
    expect(keyTokens('')).toEqual([]);
    expect(nodeAtKey(miniMap.root, '')).toBe(miniMap.root);
    expect(nodeAtKey(miniMap.root, twins[1].key + '\u0001tool:nope')).toBeNull();
  });

  it('opens the branch a fork or hotspot entry describes, where two read the same', () => {
    const [mini] = miniMap.root.children;
    const [first, second] = mini.children;
    const entry = { path: ['llm:gpt-4o-mini', 'tool:search×2'], visits: second.visits, success_rate: second.success_rate };
    expect(nodeAtKey(miniMap.root, entryKey(miniMap.root, entry))).toBe(second);
    expect(nodeAtKey(miniMap.root, entryKey(miniMap.root, { ...entry, success_rate: first.success_rate }))).toBe(first);
    // A path the map does not draw keys as its text.
    expect(entryKey(miniMap.root, { path: ['tool:nope'] })).toBe(pathKey(['tool:nope']));
    expect(entryKey(supportMap.root, researcherMap.fatal_forks[0])).toBe(pathKey(researcherMap.fatal_forks[0].path));
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

  it('places nothing on a map drawn shorter than where foresight judged the run, and says how far it got', () => {
    // The same store, the map asked for at depth 2: foresight matched the
    // healthy run at depth 4, past the last drawn column.
    const cut = runOnMap(supportDepth2.root, healthy);
    expect(cut.complete).toBe(false);
    expect(cut.walked).toBe(2);
    expect(cut.matchedDepth).toBe(4);
    expect(cut.atKey).toBeNull();
    expect(cut.reachedKey).toBe(pathKey(['llm:gpt-4o', 'tool:search_orders']));
    expect([...cut.keys].sort()).toEqual([pathKey([]), pathKey(['llm:gpt-4o']), pathKey(['llm:gpt-4o', 'tool:search_orders'])].sort());
    expect(cut.pooled.size).toBe(0);
    expect(cut.recommendedKey).toBeNull();
    // Pruned rather than cut: at min support 100 the doomed run's loop
    // (50 runs) is not drawn, so no shallower node stands in for it.
    const pruned = runOnMap(supportMin100.root, doomed);
    expect(pruned.complete).toBe(false);
    expect(pruned.walked).toBe(1);
    expect(pruned.atKey).toBeNull();
    // A novel run's divergence is drawn only from the node it diverged at.
    const ghost = runOnMap(supportMin100.root, novel);
    expect(ghost.complete).toBe(true);
    expect(ghost.atKey).toBe(pathKey(['llm:gpt-4o']));
    expect(ghost.divergence.took).toBe('tool:delete_database');
    expect(runOnMap(supportMap.root, healthy).complete).toBe(true);
  });

  it('turns a path into the TQL that finds the runs which took it: signatures, the last repeat held to its bucket', () => {
    expect(pathToTql(['llm:gpt-4o', 'tool:search_orders×3-5'])).toBe(
      '^llm[signature="llm:gpt-4o"] -> tool[signature="tool:search_orders"]{3,5}$'
      + ' | ^llm[signature="llm:gpt-4o"] -> tool[signature="tool:search_orders"]{3,5} -> step[signature!="tool:search_orders"]');
    // Six or more has no upper bound: nothing to hold the last token to.
    expect(pathToTql(['llm:gpt-4o', 'tool:issue_refund×2', 'agent:x×6+']))
      .toBe('^llm[signature="llm:gpt-4o"] -> tool[signature="tool:issue_refund"]{2} -> agent[signature="agent:x"]{6,}');
    expect(pathToTql(['tool:say "hi"\\now'])).toContain('tool[signature="tool:say \\"hi\\"\\\\now"]');
    expect(pathToTql(['llm:a-very-long-cut-name…'])).toBeNull();
    // The fold token stands for every deeper run: no query says exactly which.
    expect(pathToTql(['llm:gpt-4o', '…'])).toBeNull();
    expect(pathToTql([])).toBeNull();
    expect(pathToTql(cc.path)).toMatch(/^\^llm\[signature="llm:claude-sonnet-4-5-20250929"\] -> tool\[signature="tool:Read"\]/);
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

  it('headlines no depth scored on too few runs, and reads the skill at the depth a run was judged', () => {
    // The support map's rows, and one deep row scored on twelve runs that a
    // lucky split would make the best.
    const lucky = { depths: [...supportMap.calibration.depths, { depth: 8, runs: 180, covered: 12, brier: 0.1, baseline_brier: 0.25, skill: 0.6 }] };
    expect(bestSkill(lucky).depth).toBe(3);
    // The researcher's depth 3 reached no run at all.
    expect(bestSkill(researcherMap.calibration).depth).toBe(1);
    expect(bestSkill({ depths: [{ depth: 1, runs: 12, covered: 12, brier: 0.2, baseline_brier: 0.25, skill: 0.2 }] })).toBeNull();
    expect(skillAt(supportMap.calibration, 1).skill).toBe(0);
    expect(skillAt(supportMap.calibration, 2).skill).toBeCloseTo(0.2053);
    expect(skillAt(supportMap.calibration, 4).depth).toBe(3);
    expect(skillAt(supportMap.calibration, 40).depth).toBe(5);
    expect(skillAt(lucky, 8).depth).toBe(5);
    expect(skillAt(supportMap.calibration, 0)).toBeNull();
  });
});

describe('Find these runs', () => {
  // Each loop-agent run's own path, from its id: `s<n>` called `search` n
  // times after one gpt-4o call, `cap3` called `Search` three times.
  const bucket = (n) => (n === 1 ? '' : n === 2 ? '×2' : n <= 5 ? '×3-5' : '×6+');
  const pathOf = (run) => (run === 'cap3' ? ['llm:gpt-4o', 'tool:Search×3-5']
    : ['llm:gpt-4o', `tool:search${bucket(Number(run.slice(1)))}`]);
  const nodes = [];
  (function walk(node, path) {
    for (const child of node.children || []) {
      nodes.push({ node: child, path: [...path, child.token] });
      walk(child, [...path, child.token]);
    }
  }(loopsMap.root, []));

  it('finds exactly the runs through each node, as the server answered the query', () => {
    expect(nodes).toHaveLength(6);
    for (const { node, path } of nodes) {
      const q = pathToTql(path, loopsMap.root);
      expect(loopAnswers, `the answer to ${q} was captured`).toHaveProperty([q]);
      const runs = loopAnswers[q].matches.map((m) => m.run);
      // Every run the server matched took this node's path — six searches
      // are not three to five, and Search is not search...
      for (const run of runs) expect(pathOf(run).slice(0, path.length), `${run} found under ${path.join(' > ')}`).toEqual(path);
      // ...and every run through the node was matched.
      expect(runs, path.join(' > ')).toHaveLength(node.visits);
      for (const example of node.example_runs) expect(runs).toContain(example.run);
    }
  });

  it('offers no search where two branches of the map read the same, or a name carries the bucket mark', () => {
    // `search×2` called once and `search` called twice: one text, two branches.
    const [mini] = miniMap.root.children;
    expect(mini.children.map((c) => c.token)).toEqual(['tool:search×2', 'tool:search×2']);
    expect(pathToTql(['llm:gpt-4o-mini', 'tool:search×2'], miniMap.root)).toBeNull();
    expect(pathToTql(['llm:gpt-4o-mini', 'tool:search×2', 'llm:gpt-4o-mini'], miniMap.root)).toBeNull();
    expect(pathToTql(['llm:gpt-4o-mini'], miniMap.root)).not.toBeNull();
    // A name that itself holds the mark: where it ends cannot be read.
    expect(pathToTql(['tool:x×2×2'])).toBeNull();
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

  it('keeps the shared prefix of a broken call servable: only past the TTL is it lost too', () => {
    // Step 2 broke on a clock in the system prompt: 154 of its 254 tokens
    // lead byte-identical to step 0, and ~34 (129 at the provider's scale)
    // of what followed the divergence were re-processed.
    const volatile = autopsy.steps.find((s) => s.step_index === 2);
    expect(volatile.cause).toBe('volatile_content');
    expect(cacheSegments(volatile)).toEqual({ total: 254, served: 154, lost: 34, fresh: 66 });
    // A compaction (new_context with a predecessor) keeps its leading chunks too.
    const rewritten = autopsy.steps.find((s) => s.step_index === 34);
    expect(rewritten.cause).toBe('new_context');
    expect(cacheSegments(rewritten)).toEqual({ total: 219, served: 154, lost: 6, fresh: 59 });
    // The run's first call has nothing to serve from.
    expect(cacheSegments(autopsy.steps[0]).served).toBe(0);
    // Whether a strict prefix hits is unknown: nothing is drawn as servable.
    expect(cacheSegments({ ...rewritten, cause: 'unknown' }).served).toBe(0);
  });

  it('draws a partial hit as what the provider served and what it did not', () => {
    // Past the TTL the provider read half the predecessor's context: the
    // server counts the other half lost, at the provider's scale.
    const partial = {
      cause: 'partial_hit', est_tokens: 1000, shared_est_tokens: 900,
      est_tokens_lost: 1000, calibration: 2,
    };
    expect(CACHE_CAUSE.partial_hit).toEqual({ tone: 'warn', label: 'partial hit' });
    expect(cacheSegments(partial)).toEqual({ total: 1000, served: 500, lost: 500, fresh: 0 });
  });

  it('marks a held prefix the provider served nothing from', () => {
    const flagged = ccAutopsy.steps.filter(stableWithoutRead).map((s) => s.step_index);
    expect(flagged).toEqual([13]);
    expect(ccAutopsy.summary.disagreements).toBe(1);
  });
});

describe('detectors', () => {
  it("gives a detector's success rate its interval over the runs whose outcome is known", () => {
    expect(wilson(5, 50)[0]).toBeCloseTo(0.0435, 4);
    expect(wilson(5, 50)[1]).toBeCloseTo(0.2136, 4);
    expect(wilson(0, 0)).toBeNull();
    const hallucinated = findings.detectors.find((d) => d.name === 'hallucinated_tool');
    const rate = detectorRate(hallucinated, findings.baseline_success_rate);
    expect(rate.n).toBe(1);
    expect(rate.rate).toBe(0);
    expect(rate.ci[1]).toBeGreaterThan(findings.baseline_success_rate);
    // One failure in one run cannot tell the detector from the baseline.
    expect(rate.below).toBe(false);
    const quiet = detectorRate(findings.detectors.find((d) => d.name === 'ping_pong'), findings.baseline_success_rate);
    expect(quiet).toEqual({ rate: null, ci: null, n: 0, below: false });
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
