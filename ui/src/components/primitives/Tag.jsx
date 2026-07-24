import React from 'react';
/* status colors sit on their tints; neutral = ink on sunken. mono for measured values. */
export function Tag({ status = 'neutral', mono, children, style }) {
  const c = {
    neutral: { color: 'var(--ink-muted)', background: 'var(--bg-sunken)', border: '1px solid var(--hairline)' },
    ok: { color: 'var(--ok)', background: 'var(--ok-tint)', border: '1px solid transparent' },
    warn: { color: 'var(--warn)', background: 'var(--warn-tint)', border: '1px solid transparent' },
    error: { color: 'var(--error)', background: 'var(--error-tint)', border: '1px solid transparent' },
    accent: { color: 'var(--accent-hover)', background: 'var(--accent-tint)', border: '1px solid transparent' },
  }[status];
  return <span style={{ display: 'inline-flex', alignItems: 'center', gap: '4px', padding: '1px 6px', borderRadius: 'var(--radius-control)', fontSize: 'var(--text-12)', lineHeight: 'var(--lh-12)', fontWeight: 500, fontFamily: mono ? 'var(--font-mono)' : 'var(--font-sans)', fontVariantNumeric: 'tabular-nums', whiteSpace: 'nowrap', ...c, ...style }}>{children}</span>;
}
