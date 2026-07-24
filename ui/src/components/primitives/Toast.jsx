import React from 'react';
/* Inline toast card — position it yourself (kits pin bottom-right). Flat, hairline; status shows as a colored title. */
export function Toast({ status = 'neutral', title, detail, onDismiss, style }) {
  const color = { neutral: 'var(--ink)', ok: 'var(--ok)', warn: 'var(--warn)', error: 'var(--error)' }[status];
  return <div style={{ background: 'var(--bg-raised)', border: '1px solid var(--hairline)', borderRadius: 'var(--radius-card)', padding: '10px 12px', display: 'flex', gap: '12px', alignItems: 'flex-start', maxWidth: 380, fontFamily: 'var(--font-sans)', ...style }}>
    <div style={{ minWidth: 0 }}>
      <div style={{ fontSize: 'var(--text-13)', lineHeight: 'var(--lh-13)', fontWeight: 600, color }}>{title}</div>
      {detail ? <div style={{ fontSize: 'var(--text-13)', lineHeight: 'var(--lh-13)', color: 'var(--ink-muted)', marginTop: 2 }}>{detail}</div> : null}
    </div>
    {onDismiss ? <button onClick={onDismiss} style={{ marginLeft: 'auto', border: 'none', background: 'transparent', color: 'var(--ink-faint)', cursor: 'pointer', padding: 0, fontSize: 14, lineHeight: 1 }}>×</button> : null}
  </div>;
}
