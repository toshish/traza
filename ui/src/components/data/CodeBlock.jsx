import React from 'react';
import { CopyButton } from './CopyButton.jsx';
/* Sunken code block, mono 12px, copy affordance, optional language label.
   Long output scrolls inside the block rather than stretching the panel. */
export function CodeBlock({ code = '', language, copyable = true, maxHeight = 420, style }) {
  return <div style={{ position: 'relative', background: 'var(--bg-sunken)', border: '1px solid var(--hairline)', borderRadius: 'var(--radius-card)', padding: '10px 12px', fontFamily: 'var(--font-mono)', fontSize: 'var(--text-12)', lineHeight: 'var(--lh-13)', color: 'var(--ink)', minWidth: 0, ...style }}>
    <div style={{ position: 'absolute', top: 6, right: 6, display: 'flex', alignItems: 'center', gap: 6 }}>
      {language ? <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-12)', color: 'var(--ink-faint)' }}>{language}</span> : null}
      {copyable ? <CopyButton text={code} /> : null}
    </div>
    <pre style={{ margin: 0, fontFamily: 'inherit', whiteSpace: 'pre', fontVariantNumeric: 'tabular-nums', overflow: 'auto', maxHeight }}>{code}</pre>
  </div>;
}
