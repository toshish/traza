import React from 'react';
import { bytesLabel, messageText, parseLoadedMessages, toolResultParts } from '../../lib/spans.js';
import { CopyButton } from '../data/CopyButton.jsx';
import { CodeBlock } from '../data/CodeBlock.jsx';
import { MediaBlock } from '../data/MediaBlock.jsx';
import { RichText } from '../data/RichText.jsx';
import { Tag } from '../primitives/Tag.jsx';

/* A chat turn rendered part by part: prose as prose, JSON as JSON, media as
   media, tool calls as calls. Used by the conversation view and the trace
   inspector's payload modal, so a turn looks the same wherever it is read. */

const ROLE_TINT = {
  user: 'var(--accent-tint)',
  assistant: 'var(--bg-raised)',
  system: 'var(--bg-sunken)',
  tool: 'var(--bg-sunken)',
};

// An offloaded conversation up to this size is fetched without being asked:
// the reader opened this screen to SEE the turn, and a one-request fetch from
// the local payload store is cheaper than a click that every reader must
// make. Anything larger stays behind the button it always had.
const AUTO_LOAD_PAYLOAD_BYTES = 4 * (1 << 20);

// Payload bodies are content-addressed (sha256/…), so a fulfilled fetch can
// be shared forever and across screens; only failures are forgotten, so a
// retry actually retries.
const payloadCache = new Map();

function loadPayloadCached(onLoadPayload, ref) {
  let promise = payloadCache.get(ref);
  if (!promise) {
    promise = Promise.resolve(onLoadPayload(ref));
    payloadCache.set(ref, promise);
    promise.catch(() => payloadCache.delete(ref));
  }
  return promise;
}

function SmallButton({ onClick, disabled, children }) {
  return <button onClick={onClick} disabled={disabled}
    style={{ border: '1px solid var(--hairline)', borderRadius: 'var(--radius-control)', background: 'transparent', color: 'var(--ink-muted)', cursor: disabled ? 'default' : 'pointer', fontFamily: 'var(--font-sans)', fontSize: 'var(--text-12)', padding: '1px 8px' }}>
    {children}
  </button>;
}

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

function ToolResult({ part, onLoadPayload }) {
  // A tool that answers with a content list (MCP tools, screenshots) gets its
  // parts rendered as parts; a plain value stays a JSON/text body.
  const parts = toolResultParts(part.result);
  const text = typeof part.result === 'string' ? part.result : JSON.stringify(part.result, null, 2);
  return <div>
    <div style={{ display: 'flex', alignItems: 'center', gap: 6, marginBottom: 4 }}>
      <Tag mono>tool result</Tag>
      {part.id ? <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-12)', color: 'var(--ink-faint)' }}>{part.id}</span> : null}
    </div>
    {parts
      ? <div style={{ display: 'grid', gap: 8, minWidth: 0 }}>
        {parts.map((sub, i) => <Part key={i} part={sub} onLoadPayload={onLoadPayload} />)}
      </div>
      : <RichText text={text} showToolbar={false} />}
  </div>;
}

function PayloadPart({ part, onLoadPayload }) {
  const [full, setFull] = React.useState(null);
  const [busy, setBusy] = React.useState(false);
  const load = async () => {
    if (!onLoadPayload) return;
    setBusy(true);
    try { setFull(await loadPayloadCached(onLoadPayload, part.ref)); } finally { setBusy(false); }
  };
  return <div>
    <div style={{ display: 'flex', alignItems: 'center', gap: 6, marginBottom: 4 }}>
      <Tag mono>offloaded {bytesLabel(part.bytes) || ''}</Tag>
      {full == null && onLoadPayload
        ? <SmallButton onClick={load} disabled={busy}>{busy ? 'loading' : 'load full'}</SmallButton>
        : null}
    </div>
    <RichText text={full ?? part.preview} showToolbar={full != null} />
    {full == null ? <div style={{ fontSize: 'var(--text-12)', color: 'var(--ink-faint)', marginTop: 2 }}>preview — the full body is in the payload store</div> : null}
  </div>;
}

function Part({ part, onLoadPayload }) {
  switch (part.kind) {
    case 'text': return <RichText text={part.text} />;
    case 'media': return <MediaBlock part={part} onLoadPayload={onLoadPayload} />;
    case 'tool_call': return <ToolCall part={part} />;
    case 'tool_result': return <ToolResult part={part} onLoadPayload={onLoadPayload} />;
    case 'payload': return <PayloadPart part={part} onLoadPayload={onLoadPayload} />;
    default: return null;
  }
}

/** The message bubble frame, shared by real turns and the offloaded-turn
    placeholder so both read as the same object. */
function Bubble({ role, direction, finishReason, meta, copyText, anchorRef, children }) {
  return <div ref={anchorRef} style={{ border: '1px solid var(--hairline)', borderLeft: '2px solid ' + (direction === 'completion' ? 'var(--accent)' : 'var(--hairline)'), borderRadius: 'var(--radius-card)', background: ROLE_TINT[role] || 'var(--bg-sunken)', padding: '8px 10px', minWidth: 0 }}>
    <div style={{ display: 'flex', alignItems: 'center', gap: 8, marginBottom: 6, minWidth: 0 }}>
      <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-12)', fontWeight: 500, color: 'var(--ink)' }}>{role}</span>
      <span style={{ fontSize: 'var(--text-12)', color: 'var(--ink-faint)' }}>{direction}</span>
      {finishReason ? <Tag mono>{finishReason}</Tag> : null}
      {meta ? <span style={{ fontSize: 'var(--text-12)', color: 'var(--ink-faint)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{meta}</span> : null}
      {copyText != null ? <span style={{ marginLeft: 'auto', flexShrink: 0 }}><CopyButton text={copyText} /></span> : null}
    </div>
    <div style={{ display: 'grid', gap: 8, minWidth: 0 }}>{children}</div>
  </div>;
}

/** One message bubble: role, direction, and its parts. */
export function Message({ message, meta, onLoadPayload }) {
  const role = message.role || (message.direction === 'completion' ? 'assistant' : 'user');
  return <Bubble role={role} direction={message.direction} finishReason={message.finishReason}
    meta={meta} copyText={messageText(message)}>
    {(message.parts || []).map((part, i) => <Part key={i} part={part} onLoadPayload={onLoadPayload} />)}
  </Bubble>;
}

/** A whole conversation attribute that was offloaded at ingest. The payload
    body is the original messages JSON, so once fetched it renders as the
    turns it always was — media included — rather than as a JSON wall. Small
    bodies fetch themselves; large ones keep the explicit button. */
function OffloadedTurn({ message, meta, onLoadPayload }) {
  const part = message.parts[0];
  const auto = Boolean(onLoadPayload) && (part.bytes || 0) <= AUTO_LOAD_PAYLOAD_BYTES;
  const [state, setState] = React.useState({ phase: auto ? 'loading' : 'idle' });
  const anchor = React.useRef(null);

  const begin = React.useCallback(() => {
    if (!onLoadPayload) return;
    setState({ phase: 'loading' });
    loadPayloadCached(onLoadPayload, part.ref).then(
      (text) => {
        const messages = parseLoadedMessages(text, message.direction);
        setState(messages ? { phase: 'parsed', messages } : { phase: 'text', text });
      },
      (error) => setState({ phase: 'error', what: (error && error.what) || 'The payload fetch failed.' }),
    );
  }, [onLoadPayload, part.ref, message.direction]);

  // Auto-load fires when the turn approaches the viewport, not at mount: a
  // hundred-turn session must not open with a hundred concurrent fetches.
  React.useEffect(() => {
    if (!auto) return undefined;
    if (typeof IntersectionObserver === 'undefined' || !anchor.current) { begin(); return undefined; }
    const observer = new IntersectionObserver((entries) => {
      if (entries.some((entry) => entry.isIntersecting)) {
        observer.disconnect();
        begin();
      }
    }, { rootMargin: '600px 0px' });
    observer.observe(anchor.current);
    return () => observer.disconnect();
  }, [auto, begin]);

  if (state.phase === 'parsed') {
    return <>{state.messages.map((m, i) => <Message key={i} message={m} onLoadPayload={onLoadPayload} />)}</>;
  }

  const label = message.direction === 'completion' ? 'output' : 'input';
  return <Bubble anchorRef={anchor} role={label} direction={message.direction} meta={meta}
    copyText={state.phase === 'text' ? state.text : part.preview}>
    <div>
      <div style={{ display: 'flex', alignItems: 'center', gap: 6, marginBottom: 4 }}>
        <Tag mono>offloaded {bytesLabel(part.bytes) || ''}</Tag>
        {state.phase === 'loading' ? <span style={{ fontSize: 'var(--text-12)', color: 'var(--ink-faint)' }}>loading…</span> : null}
        {state.phase === 'idle' && onLoadPayload
          ? <SmallButton onClick={begin}>load full</SmallButton> : null}
        {state.phase === 'error' && onLoadPayload
          ? <SmallButton onClick={begin}>retry</SmallButton> : null}
      </div>
      {state.phase === 'error'
        ? <div style={{ fontSize: 'var(--text-12)', color: 'var(--error)' }}>{state.what}</div> : null}
      {state.phase === 'text'
        ? <RichText text={state.text} />
        : <RichText text={part.preview} showToolbar={false} />}
      {state.phase === 'idle'
        ? <div style={{ fontSize: 'var(--text-12)', color: 'var(--ink-faint)', marginTop: 2 }}>preview — the full conversation is in the payload store</div> : null}
    </div>
  </Bubble>;
}

/** An ordered run of messages. */
export function MessageList({ messages, metaFor, onLoadPayload, style }) {
  if (!messages || !messages.length) return null;
  return <div style={{ display: 'grid', gap: 8, minWidth: 0, ...style }}>
    {messages.map((message, i) => message.offloadedMessages
      ? <OffloadedTurn key={i} message={message} meta={metaFor ? metaFor(message, i) : undefined} onLoadPayload={onLoadPayload} />
      : <Message key={i} message={message} meta={metaFor ? metaFor(message, i) : undefined} onLoadPayload={onLoadPayload} />)}
  </div>;
}

/** A loaded payload body, rendered as what it is: a messages attribute as its
    turns, media bytes as media, anything else as code — with the literal
    bytes one toggle away. Used by the trace inspector's payload modal. */
export function PayloadBody({ text, hintKey, onLoadPayload }) {
  const [raw, setRaw] = React.useState(false);
  const direction = /output|completion/i.test(hintKey || '') ? 'completion' : 'prompt';
  const messages = React.useMemo(() => parseLoadedMessages(text, direction), [text, direction]);
  const dataUri = React.useMemo(() => {
    const trimmed = String(text).trim();
    return /^data:/i.test(trimmed) ? trimmed : null;
  }, [text]);

  const rendered = messages
    ? <MessageList messages={messages} onLoadPayload={onLoadPayload} />
    : dataUri
      ? <MediaBlock part={mediaPartFromDataUri(dataUri)} />
      : null;

  if (!rendered) return <CodeBlock code={text} style={{ maxHeight: '55vh', whiteSpace: 'pre-wrap' }} />;

  return <div style={{ minWidth: 0 }}>
    <div style={{ display: 'flex', alignItems: 'center', gap: 6, marginBottom: 6 }}>
      <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-12)', color: 'var(--ink-faint)' }}>
        {messages ? 'messages' : 'media'}
      </span>
      <span style={{ marginLeft: 'auto', display: 'flex', gap: 6 }}>
        <SmallButton onClick={() => setRaw(!raw)}>{raw ? 'rendered' : 'raw'}</SmallButton>
        <CopyButton text={text} label="copy" />
      </span>
    </div>
    {raw
      ? <CodeBlock code={text} style={{ maxHeight: '55vh', whiteSpace: 'pre-wrap' }} />
      : <div style={{ maxHeight: '55vh', overflowY: 'auto' }}>{rendered}</div>}
  </div>;
}

function mediaPartFromDataUri(uri) {
  const mime = (uri.match(/^data:([^;,]+)/i) || [])[1];
  const mediaType = typeof mime === 'string' && mime.startsWith('image/') ? 'image'
    : typeof mime === 'string' && mime.startsWith('audio/') ? 'audio'
      : typeof mime === 'string' && mime.startsWith('video/') ? 'video' : 'document';
  return { kind: 'media', mediaType, mime, src: uri };
}
