import React from 'react';
import { StatTile } from '../data/StatTile.jsx';
import { BarChart } from '../charts/BarChart.jsx';
/* Token/cost analytics row: stat tiles + grouped bar chart, per /v1/stats/llm. */
export function AnalyticsRow({ tiles = [], chart, chartTitle, style }) {
  return <div style={{ display: 'grid', gridTemplateColumns: `repeat(${Math.max(tiles.length, 1)}, 1fr) ${chart ? '1.6fr' : ''}`, gap: 12, alignItems: 'stretch', ...style }}>
    {tiles.map((t, i) => <StatTile key={i} {...t} />)}
    {chart ? <div style={{ background: 'var(--bg-raised)', border: '1px solid var(--hairline)', borderRadius: 'var(--radius-card)', padding: '12px 16px' }}>
      {chartTitle ? <div style={{ fontSize: 'var(--text-12)', lineHeight: 'var(--lh-12)', color: 'var(--ink-muted)', marginBottom: 8, fontFamily: 'var(--font-sans)' }}>{chartTitle}</div> : null}
      <BarChart {...chart} height={chart.height || 96} />
    </div> : null}
  </div>;
}
