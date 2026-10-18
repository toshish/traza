import React from 'react';
import { fmtDurationNs, fmtNum } from '../../lib/format.js';
import { fmtMoney } from '../../lib/runs.js';
import {
  AT_OR_ABOVE, RAMP_STEPS, ciContains, fmtCi, fmtP, fmtPct, layoutFate, nodeMeans, rateColor, shortName, splitToken,
} from '../../lib/insight.js';
import { CiBar, Kv, Swatch, Token, Tooltip } from './Bits.jsx';

// The Fate Map, drawn: every path an agent's completed runs took, left to
// right. A branch is as thick as the runs that went that way, and the
// ribbons leaving a node are stacked like a flow, so what a node passes on
// visibly adds up to what came in (less the runs that ended there). Colour
// is the success rate against the agent's base rate: ink at or above it,
// the measured ramp below it — the eye goes to where runs die. Fatal forks
// and hotspots are marked in words, never by colour alone.

/** Column width bounds: wide enough for a 22-character token label, and
    no wider than reads as one tree. */
const COL_MIN = 150;
const COL_MAX = 230;

/** A paper halo behind map text, so a ribbon passing under a label never
    strikes through it. */
const HALO = { paintOrder: 'stroke', stroke: 'var(--bg-raised)', strokeWidth: 3, strokeLinejoin: 'round' };

/** The ribbon from a parent's stacked slot to a child. */
function ribbon(x1, y1, x2, y2) {
  const dx = (x2 - x1) * 0.5;
  return `M${x1},${y1} C${x1 + dx},${y1} ${x2 - dx},${y2} ${x2},${y2}`;
}

/** Where each child's ribbon leaves its parent: stacked top to bottom
    within the parent's own width, in the children's order. */
function slots(layout) {
  const out = new Map();
  const byParent = new Map();
  for (const edge of layout.edges) {
    if (!byParent.has(edge.from.key)) byParent.set(edge.from.key, []);
    byParent.get(edge.from.key).push(edge);
  }
  for (const [, edges] of byParent) {
    const parent = edges[0].from;
    const total = edges.reduce((sum, e) => sum + e.width, 0);
    // Children of a node carry at most its runs; if rounding made them
    // wider than the parent, spread them over their own total instead.
    let y = parent.y - Math.max(parent.stroke, total) / 2;
    for (const edge of edges) {
      out.set(edge.to.key, y + edge.width / 2);
      y += edge.width;
    }
  }
  return out;
}

/** The deepest column a tree draws, folded branches left out. */
function treeDepth(root, collapsed, path = '') {
  if (!root || (collapsed && collapsed.has(path))) return 0;
  let deepest = 0;
  for (const child of root.children || []) {
    deepest = Math.max(deepest, 1 + treeDepth(child, collapsed, path ? path + '\u0001' + child.token : child.token));
  }
  return deepest;
}

/** The legend: what thickness and colour mean, stated once. */
export function FateLegend({ base }) {
  return <div style={{ display: 'flex', flexWrap: 'wrap', alignItems: 'center', gap: '6px 14px', fontSize: 12, color: 'var(--ink-muted)' }}>
    <span style={{ display: 'inline-flex', alignItems: 'center', gap: 6 }}>
      <svg width="34" height="12" aria-hidden="true"><path d="M1,9 C12,9 18,4 33,4" stroke="var(--series-3)" strokeWidth="2" fill="none" /><path d="M1,7 C12,7 18,8 33,8" stroke="var(--series-3)" strokeWidth="6" fill="none" opacity="0.6" /></svg>
      width = runs
    </span>
    <span style={{ display: 'inline-flex', alignItems: 'center', gap: 5 }}>
      <Swatch color={AT_OR_ABOVE} /> at or above the base rate{typeof base === 'number' ? ` (${fmtPct(base)})` : ''}
    </span>
    <span style={{ display: 'inline-flex', alignItems: 'center', gap: 4 }}>
      below it by
      {RAMP_STEPS.map((step) => <span key={step.label} style={{ display: 'inline-flex', alignItems: 'center', gap: 3, marginLeft: 4 }}>
        <Swatch color={step.color} /><span style={{ fontFamily: 'var(--font-mono)', fontSize: 11 }}>{step.label}</span>
      </span>)}
      <span style={{ marginLeft: 3 }}>points</span>
    </span>
    <span>faint: the interval still includes the base rate</span>
  </div>;
}

/** The reading a hovered or focused node gets. */
function NodeReading({ entry, base }) {
  const node = entry.node;
  const means = nodeMeans(node);
  return <div style={{ display: 'grid', gap: 3 }}>
    <div style={{ marginBottom: 2, overflowWrap: 'anywhere' }}>
      {entry.depth === 0 ? <span style={{ fontFamily: 'var(--font-mono)' }}>start · every run</span> : <Token token={node.token} strong />}
    </div>
    <CiBar rate={node.success_rate} ci={node.ci95} base={base} width={150} />
    <Kv k="runs">{fmtNum(node.visits)}</Kv>
    <Kv k="success / failure / abandoned">{fmtNum(node.success)} / {fmtNum(node.failure)} / {fmtNum(node.abandoned)}</Kv>
    <Kv k="95% Wilson interval">{fmtCi(node.ci95)}</Kv>
    {typeof node.lift === 'number' ? <Kv k="lift vs base">{node.lift.toFixed(2)}×</Kv> : null}
    {means.cost != null ? <Kv k="mean cost per run">${fmtMoney(means.cost)}</Kv> : null}
    {means.durationNs != null ? <Kv k="mean duration">{fmtDurationNs(means.durationNs)}</Kv> : null}
    {node.ended ? <Kv k="runs that ended here">{fmtNum(node.ended)}</Kv> : null}
    <Kv k="entropy of next step">{(node.entropy ?? 0).toFixed(2)} bits</Kv>
    {node.fork ? <Kv k={node.fatal_fork ? 'fatal fork vs siblings' : 'vs siblings'}>
      {fmtPct(node.fork.siblings_success_rate)} of {fmtNum(node.fork.siblings_visits)} · p {fmtP(node.fork.p_value)}
    </Kv> : null}
    {node.omitted_children ? <Kv k="not drawn">{fmtNum(node.omitted_children)} branches · {fmtNum(node.omitted_visits)} runs</Kv> : null}
    {node.cut ? <Kv k="cut by the node bound">{fmtNum(node.cut)} runs</Kv> : null}
  </div>;
}

/** The map. `highlight` is `runOnMap`'s answer for the run being read, or
    null; `selected` the key of the node whose panel is open. */
export function FateMap({ map, highlight, selected, onSelect, collapsed, onToggle }) {
  const base = map.base_rate;
  const wrap = React.useRef(null);
  const scroller = React.useRef(null);
  const [hover, setHover] = React.useState(null);
  // The columns share the card's width, within bounds, so a shallow tree
  // fills the card and a deep one scrolls sideways rather than cramping.
  const [avail, setAvail] = React.useState(0);
  React.useEffect(() => {
    const node = wrap.current;
    if (!node) return undefined;
    setAvail(node.getBoundingClientRect().width);
    if (typeof ResizeObserver === 'undefined') return undefined;
    const observer = new ResizeObserver((entries) => { for (const entry of entries) setAvail(entry.contentRect.width); });
    observer.observe(node);
    return () => observer.disconnect();
  }, []);
  const depth = React.useMemo(() => treeDepth(map.root, collapsed), [map.root, collapsed]);
  const col = avail ? Math.max(COL_MIN, Math.min(COL_MAX, Math.floor((avail - 56 - 170) / Math.max(1, depth)))) : 184;
  const layout = React.useMemo(() => layoutFate(map.root, { colWidth: col, collapsed }), [map.root, collapsed, col]);
  const starts = React.useMemo(() => slots(layout), [layout]);
  // The reading floats over the card, outside the scrolling canvas, so a
  // node near the bottom does not give the canvas a vertical scrollbar.
  const at = (event, entry) => {
    const box = wrap.current?.getBoundingClientRect();
    if (!box) return;
    setHover({ key: entry.key, x: event.clientX - box.left, y: event.clientY - box.top, width: box.width });
  };
  const focusAt = (entry) => {
    const left = scroller.current ? scroller.current.scrollLeft : 0;
    const width = wrap.current ? wrap.current.getBoundingClientRect().width : layout.width;
    setHover({ key: entry.key, x: entry.x - left, y: entry.y, width });
  };
  const hovered = hover ? layout.byKey.get(hover.key) : null;
  const lit = highlight || null;
  // A run opened on a deep map: bring its node into view, a third of the
  // way in, so the canvas does not open on the part the run left behind —
  // or, on a map drawn short of that node, the deepest of its path drawn.
  const litKey = lit ? (lit.atKey != null ? lit.atKey : lit.reachedKey) : null;
  React.useEffect(() => {
    const entry = litKey != null ? layout.byKey.get(litKey) : null;
    const node = scroller.current;
    if (!entry || !node || !node.clientWidth) return;
    if (entry.x > node.scrollLeft + node.clientWidth * 0.75 || entry.x < node.scrollLeft) {
      node.scrollLeft = Math.max(0, entry.x - node.clientWidth / 3);
    }
  }, [litKey, layout]);
  const atEntry = lit && lit.atKey != null ? layout.byKey.get(lit.atKey) : null;
  // A novel run's step gets a row of its own under the tree, where its
  // dashed branch crosses no drawn node.
  const ghostY = layout.height + 14;
  const height = layout.height + (atEntry && lit.divergence ? 56 : 0);

  return <div ref={wrap} style={{ position: 'relative' }} onMouseLeave={() => setHover(null)}>
    <div ref={scroller} className="tz-scroll-x">
    <svg width={layout.width} height={height} role="group"
      aria-label={`Fate Map of ${map.agent}: ${fmtNum(map.nodes_shown)} nodes over ${fmtNum(map.runs)} completed runs`}
      style={{ display: 'block', fontFamily: 'var(--font-mono)' }}>
      <g>
        {layout.edges.map((edge) => {
          const child = edge.to.node;
          const y1 = starts.get(edge.to.key) ?? edge.from.y;
          const faint = ciContains(child.ci95, base);
          return <path key={edge.to.key} d={ribbon(edge.from.x + 4, y1, edge.to.x - 3, edge.to.y)}
            stroke={rateColor(child.success_rate, base)} strokeWidth={edge.width} fill="none"
            opacity={faint ? 0.5 : 0.92} strokeLinecap="butt" />;
        })}
      </g>

      {lit ? <g aria-hidden="true">
        {layout.edges.filter((edge) => lit.keys.has(edge.to.key)).map((edge) => <path key={'run-' + edge.to.key}
          d={ribbon(edge.from.x + 4, starts.get(edge.to.key) ?? edge.from.y, edge.to.x - 3, edge.to.y)}
          stroke="var(--accent)" strokeWidth="2" fill="none" />)}
        {layout.edges.filter((edge) => lit.pooled.has(edge.to.key) && !lit.keys.has(edge.to.key)).map((edge) => <path key={'pool-' + edge.to.key}
          d={ribbon(edge.from.x + 4, starts.get(edge.to.key) ?? edge.from.y, edge.to.x - 3, edge.to.y)}
          stroke="var(--accent)" strokeWidth="1.5" strokeDasharray="4 3" fill="none" />)}
        {atEntry && lit.divergence ? <g>
          <path d={ribbon(atEntry.x + 4, atEntry.y, atEntry.x + col - 3, ghostY)}
            stroke="var(--accent)" strokeWidth="2" strokeDasharray="5 4" fill="none" />
          <rect x={atEntry.x + col - 4} y={ghostY - 5} width="8" height="10" rx="2" fill="var(--bg-raised)" stroke="var(--accent)" strokeWidth="1.5" />
          <text x={atEntry.x + col + 9} y={ghostY - 4} fontSize="11" fill="var(--ink)" {...HALO}>{shortName(lit.divergence.took, 28)}</text>
          <text x={atEntry.x + col + 9} y={ghostY + 10} fontSize="10" fill="var(--accent-hover)" {...HALO} letterSpacing="0.06em">NEVER TAKEN HERE BEFORE</text>
        </g> : null}
      </g> : null}

      {layout.nodes.map((entry) => {
        const node = entry.node;
        const root = entry.depth === 0;
        const color = root ? 'var(--series-2)' : rateColor(node.success_rate, base);
        const h = Math.max(8, entry.stroke);
        const onRun = lit && lit.keys.has(entry.key);
        const pooled = lit && lit.pooled.has(entry.key);
        const isAt = lit && lit.atKey === entry.key;
        const recommended = lit && lit.recommendedKey === entry.key;
        const isSelected = selected === entry.key;
        const { kind, name } = splitToken(node.token);
        const top = entry.y - h / 2;
        const flags = [];
        if (node.fatal_fork) flags.push({ text: 'FATAL FORK p ' + fmtP(node.fork && node.fork.p_value), color: 'var(--error)' });
        if (node.hotspot) flags.push({ text: 'HOTSPOT ' + (node.entropy ?? 0).toFixed(2) + ' bits', color: 'var(--warn)' });
        if (recommended) flags.push({ text: 'RECOMMENDED NEXT', color: 'var(--accent-hover)' });
        if (isAt) flags.push({ text: 'THIS RUN', color: 'var(--accent-hover)' });
        const label = root ? 'start' : shortName(name, 22 - (kind ? kind.length + 1 : 0));
        return <g key={entry.key || '^'} data-testid="fate-node" data-key={entry.key} data-token={node.token}
          tabIndex={0} role="button" aria-pressed={isSelected}
          aria-label={`${root ? 'start' : node.token}: ${fmtNum(node.visits)} runs, success ${fmtPct(node.success_rate)}, 95% interval ${fmtCi(node.ci95)}${node.fatal_fork ? ', fatal fork' : ''}${node.hotspot ? ', hotspot' : ''}`}
          onClick={() => onSelect && onSelect(entry.key)}
          onDoubleClick={() => onToggle && (node.children || []).length && onToggle(entry.key)}
          onKeyDown={(e) => {
            if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); onSelect && onSelect(entry.key); }
          }}
          onMouseMove={(e) => at(e, entry)} onMouseLeave={() => setHover(null)}
          onFocus={() => focusAt(entry)}
          onBlur={() => setHover(null)}
          style={{ cursor: 'pointer', outline: 'none' }}>
          {/* A generous hit area: the label and the node, not the 6px mark. */}
          <rect x={entry.x - 6} y={top - 30} width={col - 16} height={h + 52} fill="transparent" />
          {isSelected ? <rect x={entry.x - 5} y={top - 4} width={14} height={h + 8} rx="3" fill="none" stroke="var(--ink)" strokeWidth="1.5" /> : null}
          {onRun || pooled || recommended ? <rect x={entry.x - 4} y={top - 3} width={12} height={h + 6} rx="3" fill="none"
            stroke="var(--accent)" strokeWidth={onRun ? 2 : 1.5} strokeDasharray={onRun ? undefined : '3 2'} /> : null}
          <rect x={entry.x - 1} y={top} width={6} height={h} rx="2" fill={color}
            stroke={node.fatal_fork ? 'var(--error)' : 'var(--bg-raised)'} strokeWidth={node.fatal_fork ? 1.5 : 2} />
          {node.hotspot ? <rect x={entry.x - 4} y={top - 3} width={12} height={h + 6} rx="3" fill="none"
            stroke="var(--warn)" strokeWidth="1.25" strokeDasharray="2 2" /> : null}
          <text x={entry.x + 11} y={top - 5} fontSize="11" {...HALO}>
            {kind && !root ? <tspan fill="var(--ink-faint)">{kind}:</tspan> : null}
            <tspan fill="var(--ink)" fontWeight={isSelected || isAt ? 600 : 400}>{label}</tspan>
          </text>
          <text x={entry.x + 11} y={top + h + 13} fontSize="11" {...HALO} style={{ fontVariantNumeric: 'tabular-nums' }}>
            <tspan fill={node.success_rate != null && base != null && node.success_rate < base ? 'var(--ink)' : 'var(--ink-muted)'}>{fmtPct(node.success_rate)}</tspan>
            <tspan fill="var(--ink-faint)"> · {fmtNum(node.visits)}</tspan>
            {node.omitted_children ? <tspan fill="var(--ink-faint)"> · +{fmtNum(node.omitted_children)}</tspan> : null}
            {entry.hiddenChildren ? <tspan fill="var(--ink-faint)"> · {entry.hiddenChildren} folded</tspan> : null}
          </text>
          {flags.map((flag, i) => <text key={flag.text} x={entry.x + 11} y={top - 18 - i * 12} fontSize="9.5"
            letterSpacing="0.06em" fill={flag.color} {...HALO} fontWeight="500">{flag.text}</text>)}
        </g>;
      })}
    </svg>
    </div>
    {hovered ? <Tooltip x={hover.x} y={hover.y} containerWidth={hover.width}><NodeReading entry={hovered} base={base} /></Tooltip> : null}
  </div>;
}
