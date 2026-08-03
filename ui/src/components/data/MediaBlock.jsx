import React from 'react';
import { bytesLabel, base64ToDataUri } from '../../lib/spans.js';
import { CopyButton } from './CopyButton.jsx';

/* Renders a media part from a message inline — an image as an image, audio and
   video with real controls — with download and open affordances.

   Three honest non-renderable states, each shown as what it is:
   - a locator the browser cannot fetch (s3://, gs://) is a reference with its
     URI — an object store path is a real, useful answer;
   - bytes offloaded to the payload store load on demand, media stays media;
   - bytes the emitter never captured say so, with the emitter's reason,
     because an empty frame reads as a rendering bug and this is not one. */

const ICON = {
  image: 'M3 5h18v14H3zM3 16l5-5 4 4 3-3 6 6',
  audio: 'M9 18V5l10-2v13M9 18a3 3 0 1 1-6 0 3 3 0 0 1 6 0zm10-2a3 3 0 1 1-6 0 3 3 0 0 1 6 0z',
  video: 'M3 6h13v12H3zM16 10l5-3v10l-5-3',
  document: 'M6 2h8l4 4v16H6zM14 2v4h4',
};

function Chrome({ part, children, actions }) {
  const size = bytesLabel(part.sizeBytes);
  const dims = part.width && part.height ? part.width + '×' + part.height : null;
  return <div style={{ border: '1px solid var(--hairline)', borderRadius: 'var(--radius-card)', background: 'var(--bg-sunken)', padding: 8, minWidth: 0 }}>
    <div style={{ display: 'flex', alignItems: 'center', gap: 8, marginBottom: children ? 6 : 0, minWidth: 0 }}>
      <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="var(--ink-faint)" strokeWidth="1.5" style={{ flexShrink: 0 }}>
        <path d={ICON[part.mediaType] || ICON.document} />
      </svg>
      <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-12)', color: 'var(--ink)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
        {part.filename || part.mediaType}
      </span>
      <span style={{ fontSize: 'var(--text-12)', color: 'var(--ink-faint)', whiteSpace: 'nowrap' }}>
        {[part.mime, size, dims].filter(Boolean).join(' · ')}
      </span>
      <span style={{ marginLeft: 'auto', display: 'flex', alignItems: 'center', gap: 4, flexShrink: 0 }}>{actions}</span>
    </div>
    {children}
  </div>;
}

function LinkButton({ href, download, children, title }) {
  return <a href={href} download={download} target={download ? undefined : '_blank'} rel="noreferrer noopener" title={title}
    style={{ display: 'inline-flex', alignItems: 'center', gap: 4, border: '1px solid var(--hairline)', borderRadius: 'var(--radius-control)', background: 'transparent', color: 'var(--ink-muted)', textDecoration: 'none', fontFamily: 'var(--font-sans)', fontSize: 'var(--text-12)', lineHeight: 'var(--lh-12)', padding: '2px 8px', cursor: 'pointer' }}>
    {children}
  </a>;
}

function ActionButton({ onClick, disabled, children }) {
  return <button onClick={onClick} disabled={disabled}
    style={{ border: '1px solid var(--hairline)', borderRadius: 'var(--radius-control)', background: 'transparent', color: 'var(--ink-muted)', cursor: disabled ? 'default' : 'pointer', fontFamily: 'var(--font-sans)', fontSize: 'var(--text-12)', lineHeight: 'var(--lh-12)', padding: '2px 8px' }}>
    {children}
  </button>;
}

function Note({ children }) {
  return <div style={{ fontSize: 'var(--text-12)', color: 'var(--ink-muted)' }}>{children}</div>;
}

export function MediaBlock({ part, onLoadPayload }) {
  const [broken, setBroken] = React.useState(false);
  const [fetched, setFetched] = React.useState(null); // data: URI from the payload store
  const [busy, setBusy] = React.useState(false);
  const [fetchNote, setFetchNote] = React.useState(null);
  const downloadName = part.filename || (part.mediaType + guessExtension(part.mime));
  const src = part.src || fetched;

  const loadBytes = async () => {
    setBusy(true);
    setFetchNote(null);
    try {
      const text = String(await onLoadPayload(part.payloadRef.ref)).trim();
      const uri = /^data:/i.test(text) ? text : base64ToDataUri(text, part.mime);
      if (uri) setFetched(uri);
      else setFetchNote('The stored payload is not media bytes — inspect it from the span attributes.');
    } catch (e) {
      setFetchNote(e && e.what ? e.what : 'The payload fetch failed; retry.');
    } finally {
      setBusy(false);
    }
  };

  if (!src) {
    // Bytes offloaded at ingest: fetchable on demand, still media.
    if (part.payloadRef && onLoadPayload) {
      return <Chrome part={part} actions={
        <ActionButton onClick={loadBytes} disabled={busy}>
          {busy ? 'loading' : 'load ' + (bytesLabel(part.payloadRef.bytes) || 'media')}
        </ActionButton>
      }>
        {fetchNote ? <Note>{fetchNote}</Note> : null}
      </Chrome>;
    }
    // Never captured: the absence is the emitter's statement, not a failure here.
    if (part.unavailable) {
      return <Chrome part={part}>
        <Note>
          The emitter did not capture these bytes
          {part.unavailableReason ? <> — <span style={{ fontFamily: 'var(--font-mono)' }}>{part.unavailableReason}</span></> : null}
          , so only this description travelled with the trace.
        </Note>
      </Chrome>;
    }
    // Not fetchable here: show the reference, and let the reader copy it.
    return <Chrome part={part} actions={part.uri ? <CopyButton text={part.uri} label="copy uri" /> : null}>
      {part.uri ? <div style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-12)', color: 'var(--ink-muted)', wordBreak: 'break-all' }}>{part.uri}</div> : null}
    </Chrome>;
  }

  // "open" only for http(s): browsers refuse top-frame data: navigation, so
  // for inline bytes the working affordance is download.
  const actions = <>
    <LinkButton href={src} download={downloadName} title="Download">download</LinkButton>
    {/^https?:/i.test(src) ? <LinkButton href={src} title="Open in a new tab">open</LinkButton> : null}
  </>;

  if (part.mediaType === 'image' && !broken) {
    return <Chrome part={part} actions={actions}>
      <img src={src} alt={part.filename || 'generated image'} onError={() => setBroken(true)}
        style={{ display: 'block', maxWidth: '100%', maxHeight: 320, borderRadius: 4, border: '1px solid var(--hairline)', background: 'var(--bg)', imageRendering: 'auto' }} />
    </Chrome>;
  }
  if (part.mediaType === 'audio' && !broken) {
    return <Chrome part={part} actions={actions}>
      <audio controls src={src} onError={() => setBroken(true)} style={{ width: '100%', maxWidth: 420 }} />
    </Chrome>;
  }
  if (part.mediaType === 'video' && !broken) {
    return <Chrome part={part} actions={actions}>
      <video controls src={src} onError={() => setBroken(true)}
        style={{ display: 'block', maxWidth: '100%', maxHeight: 320, borderRadius: 4, border: '1px solid var(--hairline)', background: '#000' }} />
    </Chrome>;
  }
  // Documents, and anything whose bytes failed to load.
  return <Chrome part={part} actions={actions}>
    {broken ? <Note>Could not be displayed here — download it to view.</Note> : null}
  </Chrome>;
}

function guessExtension(mime) {
  if (typeof mime !== 'string') return '';
  const tail = mime.split('/')[1];
  return tail ? '.' + tail.replace('mpeg', 'mp3').replace('svg+xml', 'svg') : '';
}
