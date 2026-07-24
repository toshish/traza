import React from 'react';
/* Line chart on hairline grid; accent measured series, optional ink@40% compare. values: number[] */
export function LineChart({ values = [], compare, height = 140, labels = [], style }) {
  const all = compare ? values.concat(compare) : values;
  const max = Math.max(...all, 1), min = Math.min(...all, 0);
  const range = Math.max(max - min, 1);
  const pts = (arr) => arr.map((v, i) => `${(i / Math.max(arr.length - 1, 1)) * 100},${100 - ((v - min) / range) * 100}`).join(' ');
  return <div style={{ fontFamily: 'var(--font-sans)', ...style }}>
    <div style={{ position: 'relative', height, borderBottom: '1px solid var(--ink)' }}>
      {[0.25, 0.5, 0.75].map((g) => <div key={g} style={{ position: 'absolute', left: 0, right: 0, bottom: `${g * 100}%`, height: 1, background: 'var(--grid)' }}></div>)}
      <svg viewBox="0 0 100 100" preserveAspectRatio="none" style={{ position: 'absolute', inset: 0, width: '100%', height: '100%' }}>
        {compare ? <polyline points={pts(compare)} fill="none" stroke="var(--series-compare)" strokeWidth="1.5" vectorEffect="non-scaling-stroke" /> : null}
        <polyline points={pts(values)} fill="none" stroke="var(--accent)" strokeWidth="1.5" vectorEffect="non-scaling-stroke" />
      </svg>
    </div>
    {labels.length ? <div style={{ display: 'flex', justifyContent: 'space-between', padding: '4px 0 0', fontSize: 'var(--text-12)', lineHeight: 'var(--lh-12)', color: 'var(--ink-muted)', fontFamily: 'var(--font-mono)', fontVariantNumeric: 'tabular-nums' }}>
      {labels.map((l, i) => <span key={i}>{l}</span>)}</div> : null}
  </div>;
}
