import React from 'react';
/* Two-column key/value grid — keys muted sans, values mono. items: [{key, value, measured, color}] */
export function KeyValuePanel({ items = [], title, style }) {
  return <div style={{ fontFamily: 'var(--font-sans)', ...style }}>
    {title ? <div style={{ fontSize: 'var(--text-12)', lineHeight: 'var(--lh-12)', fontWeight: 500, color: 'var(--ink-muted)', textTransform: 'uppercase', letterSpacing: 'var(--tracking-caps)', marginBottom: 8 }}>{title}</div> : null}
    <dl style={{ display: 'grid', gridTemplateColumns: 'max-content 1fr', gap: '4px 16px', margin: 0 }}>
      {items.map((it, i) => <React.Fragment key={i}>
        <dt style={{ color: 'var(--ink-muted)', fontSize: 'var(--text-13)', lineHeight: 'var(--lh-13)' }}>{it.key}</dt>
        <dd style={{ margin: 0, fontFamily: 'var(--font-mono)', fontSize: 'var(--text-12)', lineHeight: 'var(--lh-13)', fontVariantNumeric: 'tabular-nums', color: it.color || (it.measured ? 'var(--accent)' : 'var(--ink)'), wordBreak: 'break-all' }}>{it.value}</dd>
      </React.Fragment>)}
    </dl>
  </div>;
}
