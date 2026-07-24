import React from 'react';
/* Score/annotation chip: name (sans, muted) + value (mono, accent) + source. */
export function ScoreChip({ name, value, source, onClick, style }) {
  return <span onClick={onClick} style={{ display: 'inline-flex', alignItems: 'center', gap: 6, background: 'var(--bg-raised)', border: '1px solid var(--hairline)', borderRadius: 'var(--radius-control)', padding: '2px 8px', fontSize: 'var(--text-12)', lineHeight: 'var(--lh-12)', cursor: onClick ? 'pointer' : 'default', fontFamily: 'var(--font-sans)', whiteSpace: 'nowrap', ...style }}>
    <span style={{ color: 'var(--ink-muted)' }}>{name}</span>
    <span style={{ fontFamily: 'var(--font-mono)', fontVariantNumeric: 'tabular-nums', fontWeight: 500, color: 'var(--accent)' }}>{value}</span>
    {source ? <span style={{ color: 'var(--ink-faint)', borderLeft: '1px solid var(--hairline)', paddingLeft: 6 }}>{source}</span> : null}
  </span>;
}
