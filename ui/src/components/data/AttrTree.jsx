import React from 'react';
/* JSON / span-attribute tree. Collapsible objects; keys muted, strings ink, numbers accent (measured). */
function Node({ k, v, depth }) {
  const [open, setOpen] = React.useState(depth < 2);
  const pad = { paddingLeft: depth * 16 };
  const keyEl = k != null ? <span style={{ color: 'var(--ink-muted)' }}>{k}: </span> : null;
  if (v !== null && typeof v === 'object') {
    const isArr = Array.isArray(v);
    const entries = isArr ? v.map((x, i) => [i, x]) : Object.entries(v);
    return <div>
      <div onClick={() => setOpen(!open)} style={{ ...pad, cursor: 'pointer', userSelect: 'none' }}>
        <span style={{ color: 'var(--ink-faint)', display: 'inline-block', width: 10 }}>{open ? '▾' : '▸'}</span>
        {keyEl}<span style={{ color: 'var(--ink-faint)' }}>{isArr ? `[${entries.length}]` : `{${entries.length}}`}</span>
      </div>
      {open ? entries.map(([ck, cv]) => <Node key={ck} k={String(ck)} v={cv} depth={depth + 1} />) : null}
    </div>;
  }
  const color = typeof v === 'number' ? 'var(--accent)' : typeof v === 'boolean' ? 'var(--warn)' : 'var(--ink)';
  return <div style={{ ...pad }}><span style={{ display: 'inline-block', width: 10 }}></span>{keyEl}
    {typeof v === 'string'
      ? <LongString value={v} color={color} />
      : <span style={{ color, fontVariantNumeric: 'tabular-nums', wordBreak: 'break-all' }}>{String(v)}</span>}
  </div>;
}

/* Long string values (an inlined base64 image, a whole serialized prompt) are
   clipped: pasting 13 KB of base64 into the tree buries every other attribute
   and janks the panel. Expandable, because the value is still the truth. */
const CLIP_AT = 300;
function LongString({ value, color }) {
  const [open, setOpen] = React.useState(false);
  const long = value.length > CLIP_AT;
  const shown = !long || open ? value : value.slice(0, CLIP_AT);
  return <span style={{ color, wordBreak: 'break-all' }}>
    {`"${shown}"`}
    {long ? <button onClick={() => setOpen(!open)}
      style={{ marginLeft: 6, border: '1px solid var(--hairline)', borderRadius: 'var(--radius-control)', background: 'var(--bg-sunken)', color: 'var(--ink-muted)', font: 'inherit', fontSize: 'var(--text-12)', cursor: 'pointer', padding: '0 6px' }}>
      {open ? 'collapse' : `+${value.length - CLIP_AT} chars`}
    </button> : null}
  </span>;
}
export function AttrTree({ data, style }) {
  return <div style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-12)', lineHeight: 'var(--lh-13)', color: 'var(--ink)', ...style }}>
    <Node v={data} depth={0} />
  </div>;
}
