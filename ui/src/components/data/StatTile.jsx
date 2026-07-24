import React from 'react';
/* Big mono figure + sans label + optional delta. The figure is measured → accent. */
export function StatTile({ label, value, unit, delta, deltaGood, style }) {
  return <div style={{ background: 'var(--bg-raised)', border: '1px solid var(--hairline)', borderRadius: 'var(--radius-card)', padding: '12px 16px', fontFamily: 'var(--font-sans)', minWidth: 0, ...style }}>
    <div style={{ fontSize: 'var(--text-12)', lineHeight: 'var(--lh-12)', color: 'var(--ink-muted)', marginBottom: 4 }}>{label}</div>
    <div style={{ display: 'flex', alignItems: 'baseline', gap: 6, flexWrap: 'wrap' }}>
      <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-24)', lineHeight: 'var(--lh-24)', fontWeight: 500, fontVariantNumeric: 'tabular-nums', color: 'var(--accent)' }}>{value}</span>
      {unit ? <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-13)', lineHeight: 'var(--lh-13)', color: 'var(--ink-muted)' }}>{unit}</span> : null}
      {delta ? <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-12)', lineHeight: 'var(--lh-12)', fontVariantNumeric: 'tabular-nums', color: deltaGood == null ? 'var(--ink-muted)' : deltaGood ? 'var(--ok)' : 'var(--error)', marginLeft: 'auto' }}>{delta}</span> : null}
    </div>
  </div>;
}
