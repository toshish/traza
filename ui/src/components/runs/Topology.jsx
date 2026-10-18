import React from 'react';
import { api } from '../../lib/api.js';
import { useRead } from '../../lib/route.js';
import { fmtNum } from '../../lib/format.js';
import { tenantParams } from '../../lib/runs.js';
import { layoutTopology, sequenceStrip, worthDrawing } from '../../lib/topology.js';
import { Card, ErrorState, Mono, PanelHead, Skeleton } from '../primitives/Chrome.jsx';

// Which agents a run involved, who handed control to whom, and where two of
// them bounced it back and forth (docs/guide/steer.md#topology). Agent names
// are telemetry: drawn as SVG text nodes, which React escapes like any text.

/** The mini-graph itself: agents on a ring, handoffs as arrows with counts,
    delegations dashed, ping-pong pairs in the error colour. */
export function TopologyGraph({ doc, width = 400, height = 200 }) {
  const id = React.useId().replace(/[^a-zA-Z0-9_-]/g, '');
  const layout = React.useMemo(() => layoutTopology(doc, { width, height }), [doc, width, height]);
  const label = `Agent graph: ${layout.nodes.length} agents, ${layout.edges.filter((e) => e.kind === 'handoff').length} handoff routes`
    + (layout.cycles.length ? `, ${layout.cycles.length} ping-pong pairs` : '');
  return <svg viewBox={`0 0 ${width} ${height}`} width="100%" role="img" aria-label={label}
    style={{ display: 'block', maxHeight: height, overflow: 'visible' }}>
    <defs>
      {['ink', 'cycle'].map((tone) => <marker key={tone} id={`${id}-${tone}`} viewBox="0 0 8 8" refX="7" refY="4"
        markerWidth="7" markerHeight="7" orient="auto-start-reverse">
        <path d="M0,0 L8,4 L0,8 z" fill={tone === 'cycle' ? 'var(--error)' : 'var(--ink-muted)'} />
      </marker>)}
    </defs>
    {layout.edges.map((edge) => {
      const stroke = edge.cycle ? 'var(--error)' : edge.kind === 'delegation' ? 'var(--ink-faint)' : 'var(--ink-muted)';
      const widthPx = Math.min(3, 1 + Math.log2(Math.max(1, edge.count)) * 0.5) + (edge.cycle ? 0.4 : 0);
      return <g key={`${edge.kind}:${edge.from}:${edge.to}`} data-testid="topology-edge" data-kind={edge.kind} data-cycle={edge.cycle ? 'true' : 'false'}>
        <title>{`${edge.from} → ${edge.to}: ${edge.count} ${edge.kind === 'handoff' ? 'handoffs' : 'delegations'}${edge.errors ? `, ${edge.errors} failed` : ''}`}</title>
        <path d={`M${edge.x1},${edge.y1} Q${edge.cx},${edge.cy} ${edge.x2},${edge.y2}`} fill="none" stroke={stroke}
          strokeWidth={widthPx} strokeDasharray={edge.kind === 'delegation' ? '3 3' : undefined}
          markerEnd={edge.kind === 'handoff' ? `url(#${id}-${edge.cycle ? 'cycle' : 'ink'})` : undefined} />
        {edge.kind === 'handoff' || edge.count > 1 ? <g transform={`translate(${edge.lx},${edge.ly})`}>
          <rect x={-11} y={-7} width={22} height={14} rx={3} fill="var(--bg-raised)" stroke={edge.cycle ? 'var(--error)' : 'var(--hairline)'} />
          <text textAnchor="middle" dy="3.5" fontFamily="var(--font-mono)" fontSize="9.5"
            fill={edge.cycle ? 'var(--error)' : 'var(--ink)'} style={{ fontVariantNumeric: 'tabular-nums' }}>×{edge.count}</text>
        </g> : null}
      </g>;
    })}
    {layout.nodes.map((node) => <g key={node.id} transform={`translate(${node.x},${node.y})`} data-testid="topology-node">
      <title>{`${node.id}${node.named ? '' : ' (a service standing in for an agent)'}: ${node.steps} steps${node.errors ? `, ${node.errors} errors` : ''}`}</title>
      <rect x={-node.w / 2} y={-node.h / 2} width={node.w} height={node.h} rx={node.h / 2}
        fill="var(--bg-raised)" stroke={node.errors ? 'var(--error)' : 'var(--ink-muted)'}
        strokeDasharray={node.named ? undefined : '3 2'} strokeWidth={1} />
      <text textAnchor="middle" dy="3.8" fontFamily="var(--font-mono)" fontSize="11" fill="var(--ink)" style={{ fontVariantNumeric: 'tabular-nums' }}>
        {node.label}<tspan fill="var(--ink-faint)">{' ' + node.stepsLabel}</tspan>
      </text>
    </g>)}
  </svg>;
}

/** The Run screen's card: the graph, the ping-pong pairs, the turns. */
export function TopologyCard({ run, tenant, go }) {
  const read = useRead((signal) => api.topology(run, tenantParams(tenant), signal), [run, tenant]);
  if (read.loading && !read.data) return <Card><PanelHead title="Agents" /><Skeleton height={120} /></Card>;
  if (read.error) {
    return <Card><PanelHead title="Agents" />
      <ErrorState what={read.error.what} next={read.error.next} onRetry={read.reload} /></Card>;
  }
  const doc = read.data;
  if (!doc || !(doc.nodes || []).length) return null;
  const strip = sequenceStrip(doc);
  const layout = layoutTopology(doc, { width: 400, height: 200 });
  const cut = ['nodes', 'edges', 'handoffs', 'cycles'].filter((list) => doc[`${list}_truncated`]);
  return <Card data-testid="topology-card">
    <PanelHead title="Agents" note={`${fmtNum(doc.nodes.length)} ${doc.nodes.length === 1 ? 'agent' : 'agents'} · ${fmtNum(doc.spans_read)} spans read`} />
    {worthDrawing(doc) ? <TopologyGraph doc={doc} /> : <div style={{ fontSize: 13, color: 'var(--ink-muted)', lineHeight: '20px' }}>
      One agent, <Mono>{doc.nodes[0].agent}</Mono>{doc.nodes[0].named === false ? ' (a service standing in for one)' : ''}, and
      no handoffs. Runs whose steps name agents with <code>gen_ai.agent.name</code> draw a graph here.
    </div>}
    {layout.hidden ? <div style={{ fontSize: 11, color: 'var(--ink-faint)' }}>{layout.hidden} quieter agents not drawn.</div> : null}

    {layout.cycles.length ? <div style={{ display: 'grid', gap: 4, marginTop: 8 }}>
      {layout.cycles.map((pair) => <div key={pair.agents.join('\u0000')} data-testid="topology-cycle" style={{ display: 'flex', alignItems: 'baseline', gap: 6, fontSize: 12, flexWrap: 'wrap' }}>
        <span style={{ color: 'var(--error)', fontWeight: 500 }}>ping-pong</span>
        <Mono>{pair.agents[0]} → {pair.agents[1]} → {pair.agents[0]}</Mono>
        <Mono color="var(--accent)">×{pair.count}</Mono>
        {pair.example && go ? <a href={'#/trace/' + encodeURIComponent(pair.example.trace_id) + '?span=' + encodeURIComponent(pair.example.span_id)}
          onClick={(e) => { e.preventDefault(); go(['trace', pair.example.trace_id], { span: pair.example.span_id }); }}
          style={{ fontSize: 11, marginLeft: 'auto' }}>an example return</a> : null}
      </div>)}
    </div> : null}

    {strip.turns.length ? <div style={{ marginTop: 10 }}>
      <div style={{ fontSize: 11, color: 'var(--ink-muted)', marginBottom: 4 }}>turns, in step order</div>
      <div aria-label="Turn sequence" style={{ display: 'flex', flexWrap: 'wrap', gap: 3, alignItems: 'center' }}>
        {strip.turns.map((agent, i) => <React.Fragment key={i}>
          {i ? <span aria-hidden="true" style={{ color: 'var(--ink-faint)', fontSize: 10 }}>›</span> : null}
          <Mono size={11} color="var(--ink)" style={{ padding: '0 5px', border: '1px solid var(--hairline)', borderRadius: 'var(--radius-control)', background: 'var(--bg-sunken)' }}>{agent}</Mono>
        </React.Fragment>)}
        {strip.more || strip.cut ? <span style={{ fontSize: 11, color: 'var(--ink-faint)' }}>+{fmtNum(strip.more)} more{strip.cut ? ' (the server keeps 500)' : ''}</span> : null}
      </div>
    </div> : null}

    {doc.truncated || cut.length ? <div style={{ fontSize: 11, color: 'var(--warn)', marginTop: 8 }}>
      {doc.truncated ? 'The run has more than 50,000 spans; the earliest were read. ' : ''}
      {cut.length ? `Largest entries only: ${cut.join(', ')}.` : ''}
    </div> : null}
    {worthDrawing(doc) ? <div style={{ display: 'flex', gap: 12, flexWrap: 'wrap', marginTop: 8, fontSize: 11, color: 'var(--ink-faint)' }}>
      <span>— handoff</span><span>- - delegation</span><span style={{ color: 'var(--error)' }}>— ping-pong</span>
      <span style={{ marginLeft: 'auto' }}>figure in a pill: steps</span>
    </div> : null}
  </Card>;
}
