import React from 'react';
/* Copy-to-clipboard: quiet icon button; confirms with "copied" text for 1.2s. */
export function CopyButton({ text = '', label, style }) {
  const [done, setDone] = React.useState(false);
  const copy = () => {
    try { navigator.clipboard.writeText(text); } catch (e) {}
    setDone(true); setTimeout(() => setDone(false), 1200);
  };
  return <button onClick={copy} title="Copy"
    style={{ border: 'none', background: 'transparent', color: done ? 'var(--ok)' : 'var(--ink-faint)', cursor: 'pointer', display: 'inline-flex', alignItems: 'center', gap: 4, padding: '2px 4px', fontFamily: 'var(--font-sans)', fontSize: 'var(--text-12)', lineHeight: 'var(--lh-12)', ...style }}>
    {done ? 'copied' : <>
      <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5"><rect x="9" y="9" width="12" height="12" rx="2"></rect><path d="M5 15V5a2 2 0 0 1 2-2h10"></path></svg>
      {label}</>}
  </button>;
}
