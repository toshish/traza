import React from 'react';
/* Vertical bar chart. series: primary accent; compare series ink@40%. Ink axes, hairline gridlines.
   data: [{label, value, compare?}] */
export function BarChart({ data = [], height = 140, formatValue, unit, style }) {
  const max = Math.max(...data.map((d) => Math.max(d.value || 0, d.compare || 0)), 1);
  const fmt = formatValue || ((v) => v.toLocaleString('en-US'));
  const grid = [0.25, 0.5, 0.75];
  return <div style={{ fontFamily: 'var(--font-sans)', ...style }}>
    <div style={{ position: 'relative', height, borderBottom: '1px solid var(--ink)', display: 'flex', alignItems: 'flex-end', gap: '8%', padding: '0 4%' }}>
      {grid.map((g) => <div key={g} style={{ position: 'absolute', left: 0, right: 0, bottom: `${g * 100}%`, height: 1, background: 'var(--grid)' }}></div>)}
      {data.map((d, i) => <div key={i} style={{ flex: 1, display: 'flex', alignItems: 'flex-end', justifyContent: 'center', gap: 3, height: '100%', position: 'relative', zIndex: 1 }}>
        <div title={fmt(d.value)} style={{ width: d.compare != null ? '42%' : '60%', height: `${(d.value / max) * 100}%`, background: 'var(--accent)', borderRadius: 'var(--radius-bar) var(--radius-bar) 0 0', minHeight: 1 }}></div>
        {d.compare != null ? <div title={fmt(d.compare)} style={{ width: '42%', height: `${(d.compare / max) * 100}%`, background: 'var(--series-compare)', borderRadius: 'var(--radius-bar) var(--radius-bar) 0 0', minHeight: 1 }}></div> : null}
      </div>)}
    </div>
    <div style={{ display: 'flex', gap: '8%', padding: '4px 4% 0' }}>
      {data.map((d, i) => <div key={i} style={{ flex: 1, textAlign: 'center', fontSize: 'var(--text-12)', lineHeight: 'var(--lh-12)', color: 'var(--ink-muted)', whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis' }}>{d.label}</div>)}
    </div>
    {unit ? <div style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-12)', lineHeight: 'var(--lh-12)', color: 'var(--ink-faint)', marginTop: 2 }}>{unit}</div> : null}
  </div>;
}
