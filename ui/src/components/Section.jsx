import React from 'react';
/* Card section with an uppercase micro-label header — the dashboard kit's Section. */
export function Section({ title, action, children, style }) {
  return <section style={{ background: 'var(--bg-raised)', border: '1px solid var(--hairline)', borderRadius: 'var(--radius-card)', padding: '12px 14px', minWidth: 0, ...style }}>
    <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', marginBottom: 8 }}>
      <h2 style={{ margin: 0, fontSize: 'var(--text-12)', lineHeight: 'var(--lh-12)', fontWeight: 500, color: 'var(--ink-muted)', textTransform: 'uppercase', letterSpacing: 'var(--tracking-caps)' }}>{title}</h2>
      {action}
    </div>
    {children}
  </section>;
}
