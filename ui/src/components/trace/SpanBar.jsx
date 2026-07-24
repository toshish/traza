import React from 'react';
/* One measured span: 1.5px-radius terracotta bar on a grid track, mono duration label. */
export function SpanBar({ startPct = 0, widthPct = 100, error, label, height = 16, style }) {
  const overBar = startPct + widthPct > 82; // label would sit on the bar
  return <div style={{ position: 'relative', height, background: 'var(--bg-sunken)', borderRadius: 'var(--radius-control)', minWidth: 0, ...style }}>
    <div style={{ position: 'absolute', top: 2, bottom: 2, left: `${startPct}%`, width: `max(${widthPct}%, 2px)`, background: error ? 'var(--error)' : 'var(--accent)', borderRadius: 'var(--radius-bar)' }}></div>
    {label ? <span style={{ position: 'absolute', right: 6, top: '50%', transform: 'translateY(-50%)', fontFamily: 'var(--font-mono)', fontSize: 'var(--text-12)', lineHeight: 1, fontVariantNumeric: 'tabular-nums', color: overBar ? '#FFFFFF' : 'var(--ink-muted)' }}>{label}</span> : null}
  </div>;
}
