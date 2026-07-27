import React from 'react';
import { api } from '../lib/api.js';
import { useRead, navigate } from '../lib/route.js';
import { waterfallOrder, criticalPath, childrenOf, llmUsage } from '../lib/spans.js';
import { fmtCost, fmtDurationNs, fmtNum } from '../lib/format.js';
import { Card, Chip, Eyebrow, EmptyState, LoadingBar, Mono } from '../components/primitives/Chrome.jsx';
import { TimeRuler } from '../components/charts/Marks.jsx';

// A good run beside a bad one on a SHARED time base. Two waterfalls each
// scaled to their own duration look identical no matter how different they
// are — the whole point is that one is longer, so both are drawn against the
// longer of the two.

function useTrace(id) {
  return useRead((signal) => api.trace(id, signal), [id], { skip: !id });
}

function summarize(data) {
  const spans = data?.spans || [];
  if (!spans.length) return null;
  const t0 = Math.min(...spans.map((s) => s.start_time_ns));
  const t1 = Math.max(...spans.map((s) => s.end_time_ns));
  return {
    spans, t0, span: Math.max(1, t1 - t0),
    kids: childrenOf(spans),
    path: criticalPath(spans),
    ordered: waterfallOrder(spans),
    errors: spans.filter((s) => s.status === 'error').length,
    tokens: spans.reduce((total, s) => total + (llmUsage(s)?.totalTokens || 0), 0),
    cost: spans.reduce((total, s) => total + (llmUsage(s)?.costUsd || 0), 0),
  };
}

function Waterfall({ summary, scaleNs, label }) {
  if (!summary) {
    return <div style={{ fontSize: 13, color: 'var(--ink-muted)', padding: '12px 0' }}>Nothing loaded.</div>;
  }
  return <div>
    <TimeRuler startNs={0} endNs={scaleNs} ticks={4} />
    <div style={{ marginTop: 4 }}>
      {summary.ordered.slice(0, 60).map(({ span, depth }) => {
        const left = ((span.start_time_ns - summary.t0) / scaleNs) * 100;
        const width = Math.max(0.3, ((span.end_time_ns - span.start_time_ns) / scaleNs) * 100);
        const error = span.status === 'error';
        const onPath = summary.path.has(span.span_id);
        return <div key={span.span_id} style={{
          display: 'grid', gridTemplateColumns: '180px 1fr 72px', gap: 8,
          alignItems: 'center', minHeight: 'var(--row-h)',
        }}>
          <div style={{
            paddingLeft: 4 + depth * 10, fontSize: 'var(--cell-fs)',
            color: error ? 'var(--error)' : 'var(--ink)',
            overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
          }}>{span.name}</div>
          <div style={{ position: 'relative', height: 9 }}>
            <div style={{
              position: 'absolute', left: left + '%', width: width + '%', top: 1, height: 7,
              background: error ? 'var(--error)' : onPath ? 'var(--accent)' : 'var(--measure-2)',
              borderRadius: 'var(--radius-bar)',
            }} />
          </div>
          <div style={{
            fontFamily: 'var(--font-mono)', fontSize: 12, fontVariantNumeric: 'tabular-nums',
            textAlign: 'right', color: 'var(--ink-muted)',
          }}>{fmtDurationNs(span.end_time_ns - span.start_time_ns)}</div>
        </div>;
      })}
    </div>
  </div>;
}

export function CompareScreen({ params, go }) {
  const [a, setA] = React.useState(() => params.get('a') || '');
  const [b, setB] = React.useState(() => params.get('b') || '');
  React.useEffect(() => { navigate(['compare'], { a, b }, { replace: true }); }, [a, b]);

  const left = useTrace(a);
  const right = useTrace(b);
  const summaryA = React.useMemo(() => summarize(left.data), [left.data]);
  const summaryB = React.useMemo(() => summarize(right.data), [right.data]);
  // One scale for both, so "longer" is something you can see rather than
  // something you have to read off two axes and subtract.
  const scale = Math.max(summaryA?.span || 1, summaryB?.span || 1);

  const field = (value, onChange, label) => <input value={value}
    onChange={(event) => onChange(event.target.value.trim())} placeholder={`trace ${label}`}
    aria-label={`Trace ${label}`}
    style={{
      padding: '4px 9px', border: '1px solid var(--hairline)', borderRadius: 'var(--radius-control)',
      background: 'var(--bg)', fontFamily: 'var(--font-mono)', fontSize: 12, color: 'var(--ink)',
      outline: 'none', flex: 1, minWidth: 220,
    }} />;

  return <div style={{ display: 'grid', gap: 14, maxWidth: 1900 }}>
    <div style={{ display: 'flex', alignItems: 'center', gap: 10, flexWrap: 'wrap' }}>
      {field(a, setA, 'A')}
      {field(b, setB, 'B')}
      <Chip onClick={() => { const swap = a; setA(b); setB(swap); }}>Swap</Chip>
    </div>

    <LoadingBar active={left.loading || right.loading} />
    {!a || !b ? <EmptyState
      message="Paste two trace ids. They are drawn on one shared time base, so the slower run is visibly the slower run." /> : null}

    {summaryA && summaryB ? <Card pad="0" style={{
      display: 'grid', gridTemplateColumns: 'repeat(5,1fr)', overflow: 'hidden',
    }}>
      {[
        ['duration', fmtDurationNs(summaryA.span), fmtDurationNs(summaryB.span)],
        ['spans', fmtNum(summaryA.spans.length), fmtNum(summaryB.spans.length)],
        ['tokens', fmtNum(summaryA.tokens), fmtNum(summaryB.tokens)],
        ['cost USD', fmtCost(summaryA.cost), fmtCost(summaryB.cost)],
        ['errors', fmtNum(summaryA.errors), fmtNum(summaryB.errors)],
      ].map(([label, valueA, valueB]) => <div key={label} style={{
        padding: '10px 14px', borderRight: '1px solid var(--hairline)',
      }}>
        <div style={{ fontSize: 11, color: 'var(--ink-muted)', marginBottom: 3 }}>{label}</div>
        <div style={{ display: 'flex', alignItems: 'baseline', gap: 8 }}>
          <span style={{ fontFamily: 'var(--font-mono)', fontSize: 15, color: 'var(--accent)', fontVariantNumeric: 'tabular-nums' }}>{valueA}</span>
          <span style={{ color: 'var(--ink-faint)' }}>→</span>
          <span style={{ fontFamily: 'var(--font-mono)', fontSize: 15, color: 'var(--ink)', fontVariantNumeric: 'tabular-nums' }}>{valueB}</span>
        </div>
      </div>)}
    </Card> : null}

    <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 12 }}>
      {[[a, summaryA, left], [b, summaryB, right]].map(([id, summary, state], index) => (
        <Card key={index}>
          <div style={{ display: 'flex', alignItems: 'baseline', gap: 8, marginBottom: 10 }}>
            <Eyebrow>{index === 0 ? 'A' : 'B'}</Eyebrow>
            <Mono color="var(--ink-muted)">{id ? id.slice(0, 24) : '—'}</Mono>
            {id ? <span style={{ marginLeft: 'auto' }}>
              <Chip onClick={() => go(['trace', id])}>Open</Chip>
            </span> : null}
          </div>
          {state.error
            ? <div style={{ fontSize: 13, color: 'var(--error)' }}>
                {state.error.status === 404 ? 'Trace not found.' : state.error.what}
              </div>
            : <Waterfall summary={summary} scaleNs={scale} />}
        </Card>
      ))}
    </div>
  </div>;
}
