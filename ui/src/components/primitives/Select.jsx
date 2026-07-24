import React from 'react';
export function Select({ value, onChange, options = [], disabled, size = 'md', style }) {
  const pad = size === 'sm' ? '3px 26px 3px 8px' : '6px 28px 6px 10px';
  return <span style={{ position: 'relative', display: 'inline-flex', ...style }}>
    <select value={value} disabled={disabled} onChange={onChange ? (e) => onChange(e.target.value) : undefined}
      style={{ appearance: 'none', WebkitAppearance: 'none', background: 'var(--bg-raised)', border: '1px solid var(--hairline)', borderRadius: 'var(--radius-control)', color: 'var(--ink)', padding: pad, fontFamily: 'var(--font-sans)', fontSize: 'var(--text-13)', lineHeight: 'var(--lh-13)', cursor: disabled ? 'not-allowed' : 'pointer', opacity: disabled ? 0.45 : 1, width: '100%' }}>
      {options.map((o) => typeof o === 'string' ? <option key={o} value={o}>{o}</option> : <option key={o.value} value={o.value}>{o.label}</option>)}
    </select>
    <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5" style={{ position: 'absolute', right: 8, top: '50%', transform: 'translateY(-50%)', pointerEvents: 'none', color: 'var(--ink-faint)' }}><path d="M6 9l6 6 6-6"></path></svg>
  </span>;
}
