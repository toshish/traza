import React from 'react';

// One limit, drawn: how much of it a run has used. Usage is a measurement, so
// the bar is the measured hue and deepens along the --measure ramp as the
// run nears the limit — never a second colour for "bad". Whether the run is
// stopped is said by its verdict tag, in words; the bar only measures.

const FILL = { ok: 'var(--measure-3)', near: 'var(--measure-4)', reached: 'var(--measure-5)' };

export function UsageBar({ bar }) {
  const width = Math.max(bar.used > 0 ? 1.5 : 0, Math.min(1, bar.fraction) * 100);
  return <div data-testid={'usage-' + bar.key} style={{
    display: 'grid', gridTemplateColumns: '92px minmax(60px, 1fr) auto', gap: 10, alignItems: 'center',
  }}>
    <span style={{ fontSize: 12, color: 'var(--ink-muted)', whiteSpace: 'nowrap' }}>{bar.label}</span>
    <div role="meter" aria-label={bar.label} aria-valuemin={0} aria-valuemax={bar.max} aria-valuenow={bar.used}
      style={{ position: 'relative', height: 7, background: 'var(--bg-sunken)', borderRadius: 1.5, overflow: 'hidden' }}>
      <div style={{ height: '100%', width: width + '%', background: FILL[bar.level], borderRadius: 1.5 }} />
      {/* The 80% mark: where a run starts to warn. */}
      <div aria-hidden="true" style={{
        position: 'absolute', left: '80%', top: 0, bottom: 0, width: 1, background: 'var(--hairline)',
      }} />
    </div>
    <span style={{
      fontFamily: 'var(--font-mono)', fontSize: 12, fontVariantNumeric: 'tabular-nums', whiteSpace: 'nowrap',
      color: bar.level === 'ok' ? 'var(--ink)' : 'var(--accent)',
    }}>{bar.text}</span>
  </div>;
}
