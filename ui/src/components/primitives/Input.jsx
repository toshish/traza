import React from 'react';
export function Input({ value, defaultValue, onChange, onKeyDown, placeholder, mono, disabled, size = 'md', prefix, style, type = 'text', title }) {
  const pad = size === 'sm' ? '3px 8px' : '6px 10px';
  return <span title={title} style={{ display: 'inline-flex', alignItems: 'center', gap: '6px', background: 'var(--bg-raised)', border: '1px solid var(--hairline)', borderRadius: 'var(--radius-control)', padding: pad, opacity: disabled ? 0.45 : 1, ...style }}>
    {prefix ? <span style={{ color: 'var(--ink-faint)', display: 'inline-flex' }}>{prefix}</span> : null}
    <input type={type} value={value} defaultValue={defaultValue} placeholder={placeholder} disabled={disabled}
      onChange={onChange ? (e) => onChange(e.target.value) : undefined} onKeyDown={onKeyDown}
      style={{ border: 'none', outline: 'none', background: 'transparent', color: 'var(--ink)', width: '100%', padding: 0, fontFamily: mono ? 'var(--font-mono)' : 'var(--font-sans)', fontSize: 'var(--text-13)', lineHeight: 'var(--lh-13)', fontVariantNumeric: 'tabular-nums' }} />
  </span>;
}
