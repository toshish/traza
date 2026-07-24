import React from 'react';
export function Tabs({ tabs = [], active, onChange, style }) {
  return <div style={{ display: 'flex', gap: '16px', borderBottom: '1px solid var(--hairline)', ...style }}>
    {tabs.map((t) => {
      const id = typeof t === 'string' ? t : t.id;
      const label = typeof t === 'string' ? t : t.label;
      const is = id === active;
      return <button key={id} onClick={onChange ? () => onChange(id) : undefined}
        style={{ border: 'none', background: 'transparent', cursor: 'pointer', padding: '8px 0 7px', marginBottom: -1, fontFamily: 'var(--font-sans)', fontSize: 'var(--text-13)', lineHeight: 'var(--lh-13)', fontWeight: is ? 600 : 400, color: is ? 'var(--ink)' : 'var(--ink-muted)', borderBottom: is ? '2px solid var(--ink)' : '2px solid transparent', transition: 'color 120ms cubic-bezier(0,0,0.2,1)' }}>{label}</button>;
    })}
  </div>;
}
