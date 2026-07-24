import React from 'react';
import { parseMarkdown } from '../../lib/content.js';
import { CodeBlock } from './CodeBlock.jsx';

/* Renders a markdown subset from parsed DATA into React elements. Model output
   is untrusted, so nothing here goes through dangerouslySetInnerHTML and only
   http(s) links are made clickable. */

function Inline({ spans }) {
  return <>{spans.map((span, i) => {
    switch (span.kind) {
      case 'code':
        return <code key={i} style={{ fontFamily: 'var(--font-mono)', fontSize: '0.92em', background: 'var(--bg-sunken)', border: '1px solid var(--hairline)', borderRadius: 4, padding: '0 4px' }}>{span.text}</code>;
      case 'bold':
        return <strong key={i} style={{ fontWeight: 600, color: 'var(--ink)' }}>{span.text}</strong>;
      case 'italic':
        return <em key={i}>{span.text}</em>;
      case 'link':
        return <a key={i} href={span.href} target="_blank" rel="noreferrer noopener" style={{ color: 'var(--accent)' }}>{span.text}</a>;
      default:
        return <React.Fragment key={i}>{span.text}</React.Fragment>;
    }
  })}</>;
}

export function Markdown({ text, style }) {
  const blocks = React.useMemo(() => parseMarkdown(text), [text]);
  return <div style={{ fontFamily: 'var(--font-sans)', fontSize: 'var(--text-13)', lineHeight: 1.55, color: 'var(--ink)', minWidth: 0, ...style }}>
    {blocks.map((block, i) => {
      switch (block.type) {
        case 'heading': {
          const size = block.level <= 1 ? 'var(--text-16)' : block.level === 2 ? 'var(--text-14)' : 'var(--text-13)';
          return <div key={i} style={{ fontSize: size, fontWeight: 600, color: 'var(--ink)', margin: i ? '12px 0 4px' : '0 0 4px' }}>
            <Inline spans={block.spans} />
          </div>;
        }
        case 'paragraph':
          return <p key={i} style={{ margin: '0 0 8px' }}><Inline spans={block.spans} /></p>;
        case 'code':
          return <CodeBlock key={i} code={block.code} language={block.language} style={{ margin: '0 0 8px' }} />;
        case 'list': {
          const Tag = block.ordered ? 'ol' : 'ul';
          return <Tag key={i} style={{ margin: '0 0 8px', paddingLeft: 20 }}>
            {block.items.map((item, j) => <li key={j} style={{ margin: '0 0 2px' }}><Inline spans={item} /></li>)}
          </Tag>;
        }
        case 'quote':
          return <blockquote key={i} style={{ margin: '0 0 8px', padding: '2px 0 2px 10px', borderLeft: '2px solid var(--hairline)', color: 'var(--ink-muted)' }}>
            <Inline spans={block.spans} />
          </blockquote>;
        case 'table':
          return <div key={i} style={{ overflowX: 'auto', margin: '0 0 8px' }}>
            <table style={{ borderCollapse: 'collapse', fontSize: 'var(--text-12)' }}>
              <thead><tr>{block.header.map((cell, j) =>
                <th key={j} style={{ textAlign: 'left', padding: '3px 10px 3px 0', borderBottom: '1px solid var(--hairline)', color: 'var(--ink-muted)', fontWeight: 500, whiteSpace: 'nowrap' }}>
                  <Inline spans={cell} /></th>)}</tr></thead>
              <tbody>{block.rows.map((row, j) =>
                <tr key={j}>{row.map((cell, k) =>
                  <td key={k} style={{ padding: '3px 10px 3px 0', borderBottom: '1px solid var(--hairline)', fontVariantNumeric: 'tabular-nums', whiteSpace: 'nowrap' }}>
                    <Inline spans={cell} /></td>)}</tr>)}</tbody>
            </table>
          </div>;
        case 'rule':
          return <hr key={i} style={{ border: 0, borderTop: '1px solid var(--hairline)', margin: '10px 0' }} />;
        default:
          return null;
      }
    })}
  </div>;
}
