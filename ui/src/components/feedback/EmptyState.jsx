import React from 'react';
import { CodeBlock } from '../data/CodeBlock.jsx';
/* Empty state: instructive, one suggested command. Calm — no illustration, no exclamation. */
export function EmptyState({ message, command, style }) {
  return <div style={{ padding: '32px 24px', textAlign: 'left', maxWidth: 480, fontFamily: 'var(--font-sans)', ...style }}>
    <div style={{ fontSize: 'var(--text-13)', lineHeight: 'var(--lh-13)', color: 'var(--ink-muted)', marginBottom: command ? 12 : 0 }}>{message}</div>
    {command ? <CodeBlock code={command} /> : null}
  </div>;
}
