import React from 'react';
export function Switch({ checked, onChange, label, disabled, style }) {
  return <label style={{ display: 'inline-flex', alignItems: 'center', gap: '8px', cursor: disabled ? 'not-allowed' : 'pointer', opacity: disabled ? 0.45 : 1, fontSize: 'var(--text-13)', lineHeight: 'var(--lh-13)', ...style }}>
    <span onClick={onChange && !disabled ? () => onChange(!checked) : undefined}
      style={{ width: 28, height: 16, flexShrink: 0, borderRadius: 'var(--radius-control)', border: '1px solid ' + (checked ? 'var(--accent)' : 'var(--ink-faint)'), background: checked ? 'var(--accent)' : 'var(--bg-sunken)', position: 'relative', transition: 'background 120ms cubic-bezier(0,0,0.2,1)' }}>
      <span style={{ position: 'absolute', top: 2, left: checked ? 14 : 2, width: 10, height: 10, borderRadius: '2px', background: checked ? '#FFFFFF' : 'var(--ink-muted)', transition: 'left 120ms cubic-bezier(0,0,0.2,1)' }}></span>
    </span>
    {label ? <span>{label}</span> : null}
  </label>;
}
