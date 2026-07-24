import React from 'react';
import { detectFormat, prettyJson } from '../../lib/content.js';
import { CodeBlock } from './CodeBlock.jsx';
import { CopyButton } from './CopyButton.jsx';
import { Markdown } from './Markdown.jsx';

/* Model output rendered as what it actually is: JSON pretty-printed, markdown
   laid out, prose as prose — with a raw toggle, because the rendered form is
   an interpretation and sometimes you need the literal bytes. Copy always
   yields the ORIGINAL text, never the reformatted view. */

const TOGGLE_STYLE = {
  border: '1px solid var(--hairline)', borderRadius: 'var(--radius-control)',
  background: 'transparent', color: 'var(--ink-muted)', cursor: 'pointer',
  fontFamily: 'var(--font-sans)', fontSize: 'var(--text-12)', lineHeight: 'var(--lh-12)', padding: '1px 8px',
};

export function RichText({ text, defaultFormat, showToolbar = true, style }) {
  const format = React.useMemo(() => defaultFormat || detectFormat(text), [text, defaultFormat]);
  const [raw, setRaw] = React.useState(false);
  const canToggle = format !== 'text';

  let body;
  if (raw || format === 'text') {
    body = <div style={{ fontFamily: format === 'text' ? 'var(--font-sans)' : 'var(--font-mono)', fontSize: 'var(--text-13)', lineHeight: 1.5, color: 'var(--ink)', whiteSpace: 'pre-wrap', wordBreak: 'break-word', minWidth: 0 }}>{text}</div>;
  } else if (format === 'json') {
    body = <CodeBlock code={prettyJson(text) ?? text} language="json" copyable={false} />;
  } else {
    body = <Markdown text={text} />;
  }

  return <div style={{ minWidth: 0, ...style }}>
    {showToolbar ? <div style={{ display: 'flex', alignItems: 'center', gap: 6, marginBottom: 4 }}>
      <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-12)', color: 'var(--ink-faint)' }}>{format}</span>
      <span style={{ marginLeft: 'auto', display: 'flex', alignItems: 'center', gap: 6 }}>
        {canToggle ? <button onClick={() => setRaw(!raw)} style={TOGGLE_STYLE}>{raw ? 'rendered' : 'raw'}</button> : null}
        <CopyButton text={text} label="copy" />
      </span>
    </div> : null}
    {body}
  </div>;
}
