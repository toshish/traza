import React from 'react';
import { CopyButton } from './CopyButton.jsx';
/* Sunken code block, mono 12px, copy affordance. */
export function CodeBlock({ code = '', copyable = true, style }) {
  return <div style={{ position: 'relative', background: 'var(--bg-sunken)', border: '1px solid var(--hairline)', borderRadius: 'var(--radius-card)', padding: '10px 12px', fontFamily: 'var(--font-mono)', fontSize: 'var(--text-12)', lineHeight: 'var(--lh-13)', color: 'var(--ink)', overflow: 'auto', ...style }}>
    {copyable ? <span style={{ position: 'absolute', top: 6, right: 6 }}><CopyButton text={code} /></span> : null}
    <pre style={{ margin: 0, fontFamily: 'inherit', whiteSpace: 'pre', fontVariantNumeric: 'tabular-nums' }}>{code}</pre>
  </div>;
}
