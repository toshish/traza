import React from 'react';
import { tokenize, normalizeLanguage } from '../../lib/highlight.js';
import { CopyButton } from './CopyButton.jsx';

/* Sunken code block, mono 12px, copy affordance, optional language label, and
   syntax highlighting by language. Tokens become React elements — never HTML —
   so highlighting untrusted model output stays inert. Long output scrolls
   inside the block rather than stretching the panel. */

const TOKEN_STYLE = {
  keyword: { color: 'var(--syn-keyword)' },
  string: { color: 'var(--syn-string)' },
  number: { color: 'var(--syn-number)' },
  comment: { color: 'var(--syn-comment)', fontStyle: 'italic' },
  property: { color: 'var(--syn-property)' },
  punct: { color: 'var(--syn-punct)' },
};

export function CodeBlock({ code = '', language, copyable = true, highlight = true, maxHeight = 420, style }) {
  const tokens = React.useMemo(
    () => (highlight ? tokenize(code, language) : null),
    [code, language, highlight],
  );
  const label = normalizeLanguage(language) || (typeof language === 'string' ? language.trim() : '');

  return <div style={{ position: 'relative', background: 'var(--bg-sunken)', border: '1px solid var(--hairline)', borderRadius: 'var(--radius-card)', padding: '10px 12px', fontFamily: 'var(--font-mono)', fontSize: 'var(--text-12)', lineHeight: 'var(--lh-13)', color: 'var(--ink)', minWidth: 0, ...style }}>
    <div style={{ position: 'absolute', top: 6, right: 6, display: 'flex', alignItems: 'center', gap: 6 }}>
      {label ? <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-12)', color: 'var(--ink-faint)' }}>{label}</span> : null}
      {copyable ? <CopyButton text={code} /> : null}
    </div>
    <pre style={{ margin: 0, fontFamily: 'inherit', whiteSpace: 'pre', fontVariantNumeric: 'tabular-nums', overflow: 'auto', maxHeight }}>
      {tokens
        ? tokens.map((token, i) => (token.type === 'plain'
            ? token.text
            : <span key={i} style={TOKEN_STYLE[token.type]}>{token.text}</span>))
        : code}
    </pre>
  </div>;
}
