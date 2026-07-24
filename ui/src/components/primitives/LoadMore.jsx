import React from 'react';
/* Cursor pagination: "Load more" + a measured count of what's shown. */
export function LoadMore({ shown, total, onClick, loading, style }) {
  return <div style={{ display: 'flex', alignItems: 'center', gap: '12px', padding: '12px 0', fontFamily: 'var(--font-sans)', ...style }}>
    <button onClick={onClick} disabled={loading}
      style={{ border: '1px solid var(--hairline)', background: 'var(--bg-raised)', color: 'var(--ink)', borderRadius: 'var(--radius-control)', padding: '5px 14px', fontSize: 'var(--text-13)', lineHeight: 'var(--lh-13)', fontWeight: 500, cursor: loading ? 'wait' : 'pointer', fontFamily: 'var(--font-sans)' }}>
      {loading ? 'Loading…' : 'Load more'}</button>
    {shown != null ? <span style={{ fontSize: 'var(--text-12)', lineHeight: 'var(--lh-12)', color: 'var(--ink-muted)' }}>
      <span style={{ fontFamily: 'var(--font-mono)', fontVariantNumeric: 'tabular-nums', color: 'var(--ink)' }}>{shown.toLocaleString('en-US')}</span>
      {total != null ? <> of <span style={{ fontFamily: 'var(--font-mono)', fontVariantNumeric: 'tabular-nums', color: 'var(--ink)' }}>{total.toLocaleString('en-US')}</span></> : null} shown</span> : null}
  </div>;
}
