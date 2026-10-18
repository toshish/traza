// The agent graph of one run (GET /v1/runs/{run}/topology), laid out for a
// small drawing: agents on a ring, handoffs as arrows with their counts,
// delegations dashed, and the pairs that bounced control back and forth
// marked.
//
// The server's lists are largest first and each is bounded; the drawing
// keeps the busiest agents and says how many it left out, rather than
// cramming every node of a 256-agent swarm into 360 pixels.

/** Agents drawn at most; the rest are counted, not drawn. */
export const MAX_DRAWN = 10;

/** A node's size: a pill around its name, at the drawing's 11px mono. */
const CHAR_W = 6.6;
const NODE_H = 22;
const MAX_LABEL = 16;

export function nodeLabel(name) {
  const text = String(name || '');
  return text.length > MAX_LABEL ? text.slice(0, MAX_LABEL - 1) + '…' : text;
}

/** How many returns one way make a pair ping-pong on their own: the count a
    `max_handoff_cycles` of 2 stops at. */
export const PING_PONG_RETURNS = 2;

/** The pairs control came straight back between, as unordered pairs with
    the larger of the two directions' counts: `(A, B)` and `(B, A)` are one
    pair.

    A return is not by itself a bounce (docs/guide/steer.md): a supervisor
    fanning out — S, A, S, B, S — gets control back once from each worker,
    and no worker ever gets it straight back. A pair is `pingPong` when
    control came back both ways (A, B, A, B), or came back to one agent
    `PING_PONG_RETURNS` times. */
export function cyclePairs(doc) {
  const pairs = new Map();
  for (const cycle of (doc && doc.cycles) || []) {
    const [a, b] = cycle.agents || [];
    if (!a || !b) continue;
    const key = [a, b].sort().join('\u0000');
    const seen = pairs.get(key);
    if (!seen) {
      pairs.set(key, { agents: [a, b], count: cycle.count, example: cycle.example, directions: new Set([a]) });
      continue;
    }
    seen.directions.add(a);
    if (cycle.count > seen.count) Object.assign(seen, { agents: [a, b], count: cycle.count, example: cycle.example });
  }
  return [...pairs.values()]
    .map(({ directions, ...pair }) => ({ ...pair, pingPong: directions.size > 1 || pair.count >= PING_PONG_RETURNS }))
    .sort((x, y) => Number(y.pingPong) - Number(x.pingPong) || y.count - x.count);
}

function pairKey(a, b) {
  return [a, b].sort().join('\u0000');
}

/** Lays the graph out in a `width` × `height` box.

    Returns `{nodes, edges, cycles, hidden, width, height}`: nodes with their
    centre and pill width; edges as quadratic curves (`from`, `to`, the
    control point and where the count goes), each flagged `cycle` when its
    two agents ping-ponged; `cycles`, every pair control came straight back
    between, ping-pong or not; `hidden` agents not drawn. A run with one agent
    and no edges is drawn as that one node. */
export function layoutTopology(doc, { width = 360, height = 220, maxDrawn = MAX_DRAWN } = {}) {
  const all = ((doc && doc.nodes) || []).slice();
  // Busiest first: what the server sorts by, made explicit so the drawing
  // keeps the same agents whatever order arrives.
  all.sort((a, b) => (b.steps || 0) - (a.steps || 0) || (b.spans || 0) - (a.spans || 0) || (a.agent < b.agent ? -1 : 1));
  const drawn = all.slice(0, maxDrawn);
  const names = new Set(drawn.map((node) => node.agent));
  const count = drawn.length;
  // Pills first: the ring is as wide as the widest pill lets it be.
  const pills = drawn.map((node) => {
    const label = nodeLabel(node.agent);
    const steps = String(node.steps || 0);
    return { node, label, steps, w: Math.round((label.length + 1 + steps.length) * CHAR_W + 20) };
  });
  const widest = pills.reduce((max, pill) => Math.max(max, pill.w), 0);
  const rx = Math.max(24, width / 2 - widest / 2 - 6);
  const ry = Math.max(16, height / 2 - NODE_H / 2 - 10);
  const nodes = pills.map(({ node, label, steps, w }, i) => {
    let x = 0;
    let y = 0;
    if (count === 2) {
      x = i === 0 ? -rx : rx;
    } else if (count > 2) {
      // From the top, clockwise.
      const angle = -Math.PI / 2 + (2 * Math.PI * i) / count;
      x = rx * Math.cos(angle);
      y = ry * Math.sin(angle);
    }
    return {
      id: node.agent,
      label,
      stepsLabel: steps,
      x,
      y,
      w,
      h: NODE_H,
      named: node.named !== false,
      steps: node.steps || 0,
      errors: node.errors || 0,
      cost: node.cost_usd,
    };
  });
  // Centre the drawing on its bounding box: a triangle on a ring otherwise
  // sits high, with a band of nothing under it.
  if (nodes.length) {
    const minX = Math.min(...nodes.map((n) => n.x - n.w / 2));
    const maxX = Math.max(...nodes.map((n) => n.x + n.w / 2));
    const minY = Math.min(...nodes.map((n) => n.y - n.h / 2));
    const maxY = Math.max(...nodes.map((n) => n.y + n.h / 2));
    const dx = width / 2 - (minX + maxX) / 2;
    const dy = height / 2 - (minY + maxY) / 2;
    for (const node of nodes) { node.x += dx; node.y += dy; }
  }
  const at = new Map(nodes.map((node) => [node.id, node]));
  const pingPong = new Set(cyclePairs(doc).filter((pair) => pair.pingPong).map((pair) => pairKey(pair.agents[0], pair.agents[1])));

  const edges = [];
  const add = (from, to, count, kind, errors = 0) => {
    const a = at.get(from);
    const b = at.get(to);
    if (!a || !b || a === b) return;
    edges.push({ ...curve(a, b, kind === 'handoff' ? 22 : 40), from, to, count, kind, errors, cycle: kind === 'handoff' && pingPong.has(pairKey(from, to)) });
  };
  for (const handoff of (doc && doc.handoffs) || []) add(handoff.from, handoff.to, handoff.count, 'handoff');
  for (const edge of (doc && doc.edges) || []) add(edge.from, edge.to, edge.count, 'delegation', edge.errors || 0);

  return {
    nodes,
    edges,
    cycles: cyclePairs(doc),
    hidden: Math.max(0, all.length - drawn.length),
    width,
    height,
  };
}

/** A curve from `a` to `b`, bowed `bend` pixels to the left of travel, so
    the two directions between one pair of agents run side by side instead
    of on top of each other. Its ends stop at the pills' edges. */
function curve(a, b, bend) {
  const dx = b.x - a.x;
  const dy = b.y - a.y;
  const len = Math.hypot(dx, dy) || 1;
  const nx = -dy / len;
  const ny = dx / len;
  const mx = (a.x + b.x) / 2 + nx * bend;
  const my = (a.y + b.y) / 2 + ny * bend;
  const start = edgePoint(a, mx, my);
  const end = edgePoint(b, mx, my);
  // The count sits on the curve's midpoint: B(0.5) of the quadratic.
  const lx = 0.25 * start.x + 0.5 * mx + 0.25 * end.x;
  const ly = 0.25 * start.y + 0.5 * my + 0.25 * end.y;
  return { x1: start.x, y1: start.y, cx: mx, cy: my, x2: end.x, y2: end.y, lx, ly };
}

/** Where the line from a node's centre toward (tx, ty) leaves its pill,
    taking the pill as an ellipse a little larger than the box. */
function edgePoint(node, tx, ty) {
  const dx = tx - node.x;
  const dy = ty - node.y;
  if (!dx && !dy) return { x: node.x, y: node.y };
  const ex = node.w / 2 + 3;
  const ey = node.h / 2 + 3;
  // (s·dx, s·dy) lies on the ellipse; never past the target itself.
  const s = Math.min(1, 1 / Math.sqrt((dx * dx) / (ex * ex) + (dy * dy) / (ey * ey)));
  return { x: node.x + dx * s, y: node.y + dy * s };
}

/** The turn sequence cut for a strip: the first `max` turns, and how many
    more there were (the server itself keeps at most 500). */
export function sequenceStrip(doc, max = 24) {
  const turns = (doc && doc.sequence) || [];
  return { turns: turns.slice(0, max), more: Math.max(0, turns.length - max), cut: Boolean(doc && doc.sequence_truncated) };
}

/** Whether the graph is worth drawing: two agents, or any edge. One agent
    with no handoffs is a sentence, not a picture. */
export function worthDrawing(doc) {
  const nodes = (doc && doc.nodes) || [];
  return nodes.length > 1 || ((doc && doc.edges) || []).length > 0 || ((doc && doc.handoffs) || []).length > 0;
}
