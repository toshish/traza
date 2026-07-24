import React from 'react';
export function Modal({ open, title, children, footer, onClose, width = 480 }) {
  if (!open) return null;
  return <div onClick={onClose} style={{ position: 'fixed', inset: 0, background: 'rgba(31,27,23,0.4)', display: 'flex', alignItems: 'center', justifyContent: 'center', zIndex: 50 }}>
    <div onClick={(e) => e.stopPropagation()} style={{ background: 'var(--bg-raised)', border: '1px solid var(--hairline)', borderRadius: 'var(--radius-card)', width, maxWidth: '90vw', maxHeight: '85vh', display: 'flex', flexDirection: 'column', fontFamily: 'var(--font-sans)' }}>
      <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', padding: '12px 16px', borderBottom: '1px solid var(--hairline)' }}>
        <span style={{ fontSize: 'var(--text-14)', lineHeight: 'var(--lh-14)', fontWeight: 600, color: 'var(--ink)' }}>{title}</span>
        <button onClick={onClose} style={{ border: 'none', background: 'transparent', color: 'var(--ink-faint)', cursor: 'pointer', fontSize: 16, lineHeight: 1, padding: 0 }}>×</button>
      </div>
      <div style={{ padding: '16px', overflow: 'auto', color: 'var(--ink)', fontSize: 'var(--text-13)', lineHeight: 'var(--lh-13)' }}>{children}</div>
      {footer ? <div style={{ display: 'flex', justifyContent: 'flex-end', gap: '8px', padding: '12px 16px', borderTop: '1px solid var(--hairline)' }}>{footer}</div> : null}
    </div>
  </div>;
}
