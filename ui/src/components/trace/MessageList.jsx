import React from 'react';
import { bytesLabel, messageText } from '../../lib/spans.js';
import { CopyButton } from '../data/CopyButton.jsx';
import { CodeBlock } from '../data/CodeBlock.jsx';
import { MediaBlock } from '../data/MediaBlock.jsx';
import { RichText } from '../data/RichText.jsx';
import { Tag } from '../primitives/Tag.jsx';

/* A chat turn rendered part by part: prose as prose, JSON as JSON, media as
   media, tool calls as calls. Used by the span detail and by the conversation
   view, so a turn looks the same wherever it is read. */

const ROLE_TINT = {
  user: 'var(--accent-tint)',
  assistant: 'var(--bg-raised)',
  system: 'var(--bg-sunken)',
  tool: 'var(--bg-sunken)',
};

function ToolCall({ part }) {
  const args = typeof part.args === 'string' ? part.args : JSON.stringify(part.args, null, 2);
  // OpenAI returns arguments as a JSON string; pretty-print when it parses.
  let pretty = args;
  if (typeof part.args === 'string') {
    try { pretty = JSON.stringify(JSON.parse(part.args), null, 2); } catch (e) { /* leave as-is */ }
  }
  return <div>
    <div style={{ display: 'flex', alignItems: 'center', gap: 6, marginBottom: 4 }}>
      <Tag status="accent" mono>tool call</Tag>
      <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-12)', color: 'var(--ink)' }}>{part.name}</span>
      {part.id ? <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-12)', color: 'var(--ink-faint)' }}>{part.id}</span> : null}
    </div>
    <CodeBlock code={pretty} language="json" maxHeight={220} />
  </div>;
}

function ToolResult({ part }) {
  const text = typeof part.result === 'string' ? part.result : JSON.stringify(part.result, null, 2);
  return <div>
    <div style={{ display: 'flex', alignItems: 'center', gap: 6, marginBottom: 4 }}>
      <Tag mono>tool result</Tag>
      {part.id ? <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-12)', color: 'var(--ink-faint)' }}>{part.id}</span> : null}
    </div>
    <RichText text={text} showToolbar={false} />
  </div>;
}

function PayloadPart({ part, onLoadPayload }) {
  const [full, setFull] = React.useState(null);
  const [busy, setBusy] = React.useState(false);
  const load = async () => {
    if (!onLoadPayload) return;
    setBusy(true);
    try { setFull(await onLoadPayload(part.ref)); } finally { setBusy(false); }
  };
  return <div>
    <div style={{ display: 'flex', alignItems: 'center', gap: 6, marginBottom: 4 }}>
      <Tag mono>offloaded {bytesLabel(part.bytes) || ''}</Tag>
      {full == null && onLoadPayload
        ? <button onClick={load} disabled={busy} style={{ border: '1px solid var(--hairline)', borderRadius: 'var(--radius-control)', background: 'transparent', color: 'var(--ink-muted)', cursor: 'pointer', fontFamily: 'var(--font-sans)', fontSize: 'var(--text-12)', padding: '1px 8px' }}>
            {busy ? 'loading' : 'load full'}
          </button>
        : null}
    </div>
    <RichText text={full ?? part.preview} showToolbar={full != null} />
    {full == null ? <div style={{ fontSize: 'var(--text-12)', color: 'var(--ink-faint)', marginTop: 2 }}>preview — the full body is in the payload store</div> : null}
  </div>;
}

function Part({ part, onLoadPayload }) {
  switch (part.kind) {
    case 'text': return <RichText text={part.text} />;
    case 'media': return <MediaBlock part={part} />;
    case 'tool_call': return <ToolCall part={part} />;
    case 'tool_result': return <ToolResult part={part} />;
    case 'payload': return <PayloadPart part={part} onLoadPayload={onLoadPayload} />;
    default: return null;
  }
}

/** One message bubble: role, direction, and its parts. */
export function Message({ message, meta, onLoadPayload }) {
  const role = message.role || (message.direction === 'completion' ? 'assistant' : 'user');
  return <div style={{ border: '1px solid var(--hairline)', borderLeft: '2px solid ' + (message.direction === 'completion' ? 'var(--accent)' : 'var(--hairline)'), borderRadius: 'var(--radius-card)', background: ROLE_TINT[role] || 'var(--bg-sunken)', padding: '8px 10px', minWidth: 0 }}>
    <div style={{ display: 'flex', alignItems: 'center', gap: 8, marginBottom: 6, minWidth: 0 }}>
      <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-12)', fontWeight: 500, color: 'var(--ink)' }}>{role}</span>
      <span style={{ fontSize: 'var(--text-12)', color: 'var(--ink-faint)' }}>{message.direction}</span>
      {message.finishReason ? <Tag mono>{message.finishReason}</Tag> : null}
      {meta ? <span style={{ fontSize: 'var(--text-12)', color: 'var(--ink-faint)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{meta}</span> : null}
      <span style={{ marginLeft: 'auto', flexShrink: 0 }}><CopyButton text={messageText(message)} /></span>
    </div>
    <div style={{ display: 'grid', gap: 8, minWidth: 0 }}>
      {(message.parts || []).map((part, i) => <Part key={i} part={part} onLoadPayload={onLoadPayload} />)}
    </div>
  </div>;
}

/** An ordered run of messages. */
export function MessageList({ messages, metaFor, onLoadPayload, style }) {
  if (!messages || !messages.length) return null;
  return <div style={{ display: 'grid', gap: 8, minWidth: 0, ...style }}>
    {messages.map((message, i) =>
      <Message key={i} message={message} meta={metaFor ? metaFor(message, i) : undefined} onLoadPayload={onLoadPayload} />)}
  </div>;
}
