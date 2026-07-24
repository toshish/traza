import React from 'react';
const S = {
  base: { fontFamily: 'var(--font-sans)', fontSize: 'var(--text-13)', lineHeight: 'var(--lh-13)', fontWeight: 500, borderRadius: 'var(--radius-control)', cursor: 'pointer', display: 'inline-flex', alignItems: 'center', gap: '6px', transition: 'background 120ms cubic-bezier(0,0,0.2,1), border-color 120ms cubic-bezier(0,0,0.2,1), color 120ms cubic-bezier(0,0,0.2,1)', whiteSpace: 'nowrap', userSelect: 'none' },
  size: { sm: { padding: '3px 10px' }, md: { padding: '6px 14px' }, lg: { padding: '9px 18px', fontSize: 'var(--text-14)', lineHeight: 'var(--lh-14)' } },
};
export function Button({ variant = 'secondary', size = 'md', disabled, children, onClick, style, type = 'button', title }) {
  const [hover, setHover] = React.useState(false);
  const v = {
    primary: { background: hover && !disabled ? 'var(--accent-hover)' : 'var(--accent)', color: '#FFFFFF', border: '1px solid transparent' },
    secondary: { background: hover && !disabled ? 'var(--bg-sunken)' : 'var(--bg-raised)', color: 'var(--ink)', border: '1px solid var(--hairline)' },
    ghost: { background: hover && !disabled ? 'var(--bg-sunken)' : 'transparent', color: 'var(--ink-muted)', border: '1px solid transparent' },
    danger: { background: hover && !disabled ? '#872420' : 'var(--error)', color: '#FFFFFF', border: '1px solid transparent' },
  }[variant];
  return <button type={type} disabled={disabled} onClick={onClick} title={title}
    onMouseEnter={() => setHover(true)} onMouseLeave={() => setHover(false)}
    style={{ ...S.base, ...S.size[size], ...v, ...(disabled ? { opacity: 0.45, cursor: 'not-allowed' } : {}), ...style }}>{children}</button>;
}
