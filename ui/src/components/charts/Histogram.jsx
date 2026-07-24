import React from 'react';
/* Distribution histogram: dense accent bins, optional ink@40% overlay for comparison. */
export function Histogram({ bins = [], compare, height = 100, color = 'var(--accent)', xLabels = [], style }) {
  const max = Math.max(...bins, ...(compare || []), 1);
  return <div style={{ fontFamily: 'var(--font-sans)', ...style }}>
    <div style={{ position: 'relative', height, borderBottom: '1px solid var(--ink)', display: 'flex', alignItems: 'flex-end', gap: 1 }}>
      {[0.5].map((g) => <div key={g} style={{ position: 'absolute', left: 0, right: 0, bottom: `${g * 100}%`, height: 1, background: 'var(--grid)' }}></div>)}
      {bins.map((v, i) => <div key={i} style={{ flex: 1, position: 'relative', height: '100%', display: 'flex', alignItems: 'flex-end' }}>
        {compare ? <div style={{ position: 'absolute', left: 0, right: 0, bottom: 0, height: `${((compare[i] || 0) / max) * 100}%`, background: 'var(--series-compare)', borderRadius: 'var(--radius-bar) var(--radius-bar) 0 0' }}></div> : null}
        <div style={{ width: '100%', height: `${(v / max) * 100}%`, background: color, opacity: compare ? 0.85 : 1, borderRadius: 'var(--radius-bar) var(--radius-bar) 0 0', minHeight: v > 0 ? 1 : 0, position: 'relative' }}></div>
      </div>)}
    </div>
    {xLabels.length ? <div style={{ display: 'flex', justifyContent: 'space-between', padding: '4px 0 0', fontFamily: 'var(--font-mono)', fontSize: 'var(--text-12)', lineHeight: 'var(--lh-12)', fontVariantNumeric: 'tabular-nums', color: 'var(--ink-muted)' }}>
      {xLabels.map((l, i) => <span key={i}>{l}</span>)}</div> : null}
  </div>;
}
