import React from 'react';
export function Tooltip({ content, children, style }) {
  const [open, setOpen] = React.useState(false);
  return <span style={{ position: 'relative', display: 'inline-flex', ...style }}
    onMouseEnter={() => setOpen(true)} onMouseLeave={() => setOpen(false)}>
    {children}
    {open ? <span style={{ position: 'absolute', bottom: 'calc(100% + 6px)', left: '50%', transform: 'translateX(-50%)', background: 'var(--ink)', color: 'var(--bg)', padding: '4px 8px', borderRadius: 'var(--radius-control)', fontSize: 'var(--text-12)', lineHeight: 'var(--lh-12)', whiteSpace: 'nowrap', zIndex: 40, fontFamily: 'var(--font-sans)' }}>{content}</span> : null}
  </span>;
}
