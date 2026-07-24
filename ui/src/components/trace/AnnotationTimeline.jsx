import React from 'react';
import { ScoreChip } from './ScoreChip.jsx';
/* Annotation timeline: hairline spine, one entry per annotation, newest first. */
export function AnnotationTimeline({ items = [], style }) {
  return <div style={{ fontFamily: 'var(--font-sans)', ...style }}>
    {items.map((it, i) => <div key={i} style={{ display: 'grid', gridTemplateColumns: '90px 12px 1fr', gap: 0, alignItems: 'start' }}>
      <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-12)', lineHeight: 'var(--lh-13)', fontVariantNumeric: 'tabular-nums', color: 'var(--ink-faint)', paddingTop: 3 }}>{it.time}</span>
      <span style={{ position: 'relative', alignSelf: 'stretch' }}>
        <span style={{ position: 'absolute', left: 3, top: 0, bottom: i === items.length - 1 ? 'auto' : 0, height: i === items.length - 1 ? 12 : 'auto', width: 1, background: 'var(--hairline)' }}></span>
        <span style={{ position: 'absolute', left: 0, top: 8, width: 7, height: 3, borderRadius: 'var(--radius-bar)', background: 'var(--accent)' }}></span>
      </span>
      <div style={{ paddingBottom: 14, minWidth: 0 }}>
        <ScoreChip name={it.name} value={it.value} source={it.source} />
        {it.note ? <div style={{ fontSize: 'var(--text-13)', lineHeight: 'var(--lh-13)', color: 'var(--ink-muted)', marginTop: 4 }}>{it.note}</div> : null}
      </div>
    </div>)}
  </div>;
}
