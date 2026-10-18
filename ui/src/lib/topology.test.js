// The agent graph's layout, from GET /v1/runs/{run}/topology as a real
// 0.28.0 server answered it: a support-app run whose triage and billing
// agents bounced a ticket back and forth until a max_handoff_cycles policy
// stopped it, and a single-agent run.

import { describe, it, expect } from 'vitest';
import pingPong from '../test/fixtures/topology_pingpong.json';
import single from '../test/fixtures/topology_single.json';
// A supervisor handing a ticket to three workers in turn (supervisor,
// refunds, supervisor, shipping, supervisor, billing, supervisor), posted to
// a 0.28.0 server and its topology read back.
import fanout from '../test/fixtures/topology_fanout.json';
import { cyclePairs, layoutTopology, nodeLabel, sequenceStrip, worthDrawing } from './topology.js';

describe('the ping-pong run', () => {
  const layout = layoutTopology(pingPong, { width: 400, height: 200 });

  it('draws every agent inside the box, without overlap', () => {
    expect(layout.nodes.map((n) => n.id).sort()).toEqual(['billing', 'handle_ticket', 'triage']);
    for (const node of layout.nodes) {
      expect(node.x - node.w / 2).toBeGreaterThanOrEqual(0);
      expect(node.x + node.w / 2).toBeLessThanOrEqual(400);
      expect(node.y - node.h / 2).toBeGreaterThanOrEqual(0);
      expect(node.y + node.h / 2).toBeLessThanOrEqual(200);
    }
    const [a, b, c] = layout.nodes;
    for (const [p, q] of [[a, b], [b, c], [a, c]]) {
      const apart = Math.abs(p.x - q.x) > (p.w + q.w) / 2 || Math.abs(p.y - q.y) > (p.h + q.h) / 2;
      expect(apart).toBe(true);
    }
  });

  it('draws both handoff directions, each with its count, marked as ping-pong', () => {
    const handoffs = layout.edges.filter((e) => e.kind === 'handoff');
    expect(handoffs.map((e) => `${e.from}>${e.to}:${e.count}`).sort())
      .toEqual(pingPong.handoffs.map((h) => `${h.from}>${h.to}:${h.count}`).sort());
    expect(handoffs.every((e) => e.cycle)).toBe(true);
    // The two directions bow to opposite sides, so their counts do not stack.
    const [x, y] = handoffs;
    expect(Math.hypot(x.lx - y.lx, x.ly - y.ly)).toBeGreaterThan(20);
  });

  it('draws the root\'s delegations dashed, never as ping-pong', () => {
    const delegations = layout.edges.filter((e) => e.kind === 'delegation');
    expect(delegations.map((e) => `${e.from}>${e.to}`).sort()).toEqual(['handle_ticket>billing', 'handle_ticket>triage']);
    expect(delegations.some((e) => e.cycle)).toBe(false);
  });

  it('folds the two directions of a ping-pong into one pair, at the larger count', () => {
    expect(cyclePairs(pingPong)).toEqual([expect.objectContaining({ count: 3 })]);
    expect(cyclePairs(pingPong)[0].agents.slice().sort()).toEqual(['billing', 'triage']);
  });

  it('lists the turns, cut for a strip', () => {
    expect(sequenceStrip(pingPong).turns).toEqual(pingPong.sequence);
    expect(sequenceStrip(pingPong, 3)).toEqual({ turns: pingPong.sequence.slice(0, 3), more: pingPong.sequence.length - 3, cut: false });
  });
});

describe('a single agent', () => {
  it('is a sentence, not a picture', () => {
    expect(worthDrawing(single)).toBe(false);
    expect(worthDrawing(pingPong)).toBe(true);
    const layout = layoutTopology(single);
    expect(layout.nodes).toHaveLength(1);
    expect(layout.edges).toEqual([]);
  });
});

describe('a swarm', () => {
  it('draws the busiest agents and counts the rest', () => {
    const nodes = Array.from({ length: 14 }, (_, i) => ({ agent: `worker-${i}`, steps: 100 - i, spans: 1, named: true }));
    const handoffs = nodes.slice(1).map((n) => ({ from: 'worker-0', to: n.agent, count: 1 }));
    const layout = layoutTopology({ nodes, handoffs, edges: [], cycles: [] });
    expect(layout.nodes).toHaveLength(10);
    expect(layout.hidden).toBe(4);
    expect(layout.nodes[0].id).toBe('worker-0');
    // Edges to agents not drawn are not drawn either.
    expect(layout.edges).toHaveLength(9);
  });

  it('cuts a long agent name, and keeps it whole in the data', () => {
    expect(nodeLabel('a-very-long-agent-name-indeed')).toBe('a-very-long-age…');
    expect(nodeLabel('triage')).toBe('triage');
  });
});

describe('a supervisor fanning out', () => {
  it('is not ping-pong: each worker hands back once, and no worker ever gets control straight back', () => {
    const pairs = cyclePairs(fanout);
    expect(pairs.map((pair) => pair.agents[1]).sort()).toEqual(['billing', 'refunds', 'shipping']);
    expect(pairs.every((pair) => pair.pingPong === false)).toBe(true);
    const layout = layoutTopology(fanout, { width: 400, height: 200 });
    expect(layout.edges.filter((e) => e.kind === 'handoff')).toHaveLength(fanout.handoffs.length);
    expect(layout.edges.some((e) => e.cycle)).toBe(false);
  });

  it('calls a pair ping-pong once control has come back both ways, or twice one way', () => {
    expect(cyclePairs(pingPong)[0].pingPong).toBe(true);
    // a, b, a, b: one return each way.
    expect(cyclePairs({ cycles: [{ agents: ['a', 'b'], count: 1 }, { agents: ['b', 'a'], count: 1 }] })[0].pingPong).toBe(true);
    // s, w, s, w, s: twice back to s, as max_handoff_cycles 2 counts it.
    expect(cyclePairs({ cycles: [{ agents: ['s', 'w'], count: 2 }, { agents: ['w', 's'], count: 1 }] })[0].pingPong).toBe(true);
    expect(cyclePairs({ cycles: [{ agents: ['s', 'w'], count: 1 }] })[0].pingPong).toBe(false);
  });
});
