import React from 'react';
/* Session row card: id + activity window, then measured figures in mono. */
function fig(v) { return typeof v === 'number' ? v.toLocaleString('en-US') : v; }
export function SessionCard({ sessionId, traces, spans, llmCalls, tokens, costUsd, errors, window: win, onClick, style }) {
  const cells = [
    ['traces', fig(traces)], ['spans', fig(spans)], ['LLM calls', fig(llmCalls)],
    ['tokens', fig(tokens)], ['cost USD', typeof costUsd === 'number' ? costUsd.toFixed(4) : costUsd],
  ];
  return <div onClick={onClick}
    onMouseEnter={(e) => { if (onClick) e.currentTarget.style.background = 'var(--bg-sunken)'; }}
    onMouseLeave={(e) => { e.currentTarget.style.background = 'var(--bg-raised)'; }}
    style={{ background: 'var(--bg-raised)', border: '1px solid var(--hairline)', borderRadius: 'var(--radius-card)', padding: '10px 14px', cursor: onClick ? 'pointer' : 'default', fontFamily: 'var(--font-sans)', transition: 'background 120ms cubic-bezier(0,0,0.2,1)', ...style }}>
    <div style={{ display: 'flex', alignItems: 'baseline', gap: 10, marginBottom: 8 }}>
      <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-13)', lineHeight: 'var(--lh-13)', fontWeight: 500, color: 'var(--ink)' }}>{sessionId}</span>
      {win ? <span style={{ fontSize: 'var(--text-12)', lineHeight: 'var(--lh-12)', color: 'var(--ink-faint)' }}>{win}</span> : null}
      {errors ? <span style={{ marginLeft: 'auto', fontFamily: 'var(--font-mono)', fontSize: 'var(--text-12)', lineHeight: 'var(--lh-12)', fontVariantNumeric: 'tabular-nums', color: 'var(--error)' }}>{fig(errors)} errors</span> : null}
    </div>
    <div style={{ display: 'flex', gap: 20, flexWrap: 'wrap' }}>
      {cells.map(([label, value]) => <span key={label} style={{ display: 'inline-flex', alignItems: 'baseline', gap: 5 }}>
        <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-13)', lineHeight: 'var(--lh-13)', fontVariantNumeric: 'tabular-nums', color: 'var(--accent)' }}>{value}</span>
        <span style={{ fontSize: 'var(--text-12)', lineHeight: 'var(--lh-12)', color: 'var(--ink-muted)' }}>{label}</span>
      </span>)}
    </div>
  </div>;
}
