import React from 'react';
/* Error state: what happened + what to do next. No blame, no "Oops". */
export function ErrorState({ what, next, style }) {
  return <div style={{ padding: '16px', background: 'var(--error-tint)', border: '1px solid var(--hairline)', borderRadius: 'var(--radius-card)', maxWidth: 520, fontFamily: 'var(--font-sans)', ...style }}>
    <div style={{ fontSize: 'var(--text-13)', lineHeight: 'var(--lh-13)', fontWeight: 600, color: 'var(--error)', marginBottom: next ? 4 : 0 }}>{what}</div>
    {next ? <div style={{ fontSize: 'var(--text-13)', lineHeight: 'var(--lh-13)', color: 'var(--ink)' }}>{next}</div> : null}
  </div>;
}
