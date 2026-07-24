import React from 'react';
import { bytesLabel } from '../../lib/spans.js';
import { CopyButton } from './CopyButton.jsx';

/* Renders a media part from a message inline — an image as an image, audio and
   video with real controls — with download and open affordances.

   A locator the browser cannot fetch (s3://, gs://) is shown as a reference
   with its URI, rather than a broken <img>: an object store path is a real,
   useful answer and pretending it is a rendering failure helps nobody. */

const ICON = {
  image: 'M3 5h18v14H3zM3 16l5-5 4 4 3-3 6 6',
  audio: 'M9 18V5l10-2v13M9 18a3 3 0 1 1-6 0 3 3 0 0 1 6 0zm10-2a3 3 0 1 1-6 0 3 3 0 0 1 6 0z',
  video: 'M3 6h13v12H3zM16 10l5-3v10l-5-3',
  document: 'M6 2h8l4 4v16H6zM14 2v4h4',
};

function Chrome({ part, children, actions }) {
  const size = bytesLabel(part.sizeBytes);
  return <div style={{ border: '1px solid var(--hairline)', borderRadius: 'var(--radius-card)', background: 'var(--bg-sunken)', padding: 8, minWidth: 0 }}>
    <div style={{ display: 'flex', alignItems: 'center', gap: 8, marginBottom: children ? 6 : 0, minWidth: 0 }}>
      <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="var(--ink-faint)" strokeWidth="1.5" style={{ flexShrink: 0 }}>
        <path d={ICON[part.mediaType] || ICON.document} />
      </svg>
      <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-12)', color: 'var(--ink)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
        {part.filename || part.mediaType}
      </span>
      <span style={{ fontSize: 'var(--text-12)', color: 'var(--ink-faint)', whiteSpace: 'nowrap' }}>
        {[part.mime, size].filter(Boolean).join(' · ')}
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

export function MediaBlock({ part }) {
  const [broken, setBroken] = React.useState(false);
  const downloadName = part.filename || (part.mediaType + guessExtension(part.mime));

  // Not fetchable here: show the reference, and let the reader copy it.
  if (!part.src) {
    return <Chrome part={part} actions={part.uri ? <CopyButton text={part.uri} label="copy uri" /> : null}>
      {part.uri ? <div style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-12)', color: 'var(--ink-muted)', wordBreak: 'break-all' }}>{part.uri}</div> : null}
    </Chrome>;
  }

  const actions = <>
    <LinkButton href={part.src} download={downloadName} title="Download">download</LinkButton>
    <LinkButton href={part.src} title="Open in a new tab">open</LinkButton>
  </>;

  if (part.mediaType === 'image' && !broken) {
    return <Chrome part={part} actions={actions}>
      <img src={part.src} alt={part.filename || 'generated image'} onError={() => setBroken(true)}
        style={{ display: 'block', maxWidth: '100%', maxHeight: 320, borderRadius: 4, border: '1px solid var(--hairline)', background: 'var(--bg)', imageRendering: 'auto' }} />
    </Chrome>;
  }
  if (part.mediaType === 'audio' && !broken) {
    return <Chrome part={part} actions={actions}>
      <audio controls src={part.src} onError={() => setBroken(true)} style={{ width: '100%', maxWidth: 420 }} />
    </Chrome>;
  }
  if (part.mediaType === 'video' && !broken) {
    return <Chrome part={part} actions={actions}>
      <video controls src={part.src} onError={() => setBroken(true)}
        style={{ display: 'block', maxWidth: '100%', maxHeight: 320, borderRadius: 4, border: '1px solid var(--hairline)', background: '#000' }} />
    </Chrome>;
  }
  // Documents, and anything whose bytes failed to load.
  return <Chrome part={part} actions={actions}>
    {broken ? <div style={{ fontSize: 'var(--text-12)', color: 'var(--ink-muted)' }}>
      Could not be displayed here — download it to view.
    </div> : null}
  </Chrome>;
}

function guessExtension(mime) {
  if (typeof mime !== 'string') return '';
  const tail = mime.split('/')[1];
  return tail ? '.' + tail.replace('mpeg', 'mp3').replace('svg+xml', 'svg') : '';
}
