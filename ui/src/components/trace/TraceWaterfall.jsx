import React from 'react';
import { SpanBar } from './SpanBar.jsx';
/* Nested trace waterfall. spans: [{name, service, startNs, endNs, error, depth, id}] */
function fmtMs(v) { if (v >= 1000) return (v / 1000).toFixed(2) + ' s'; if (v >= 1) return v.toFixed(2) + ' ms'; return (v * 1000).toFixed(0) + ' µs'; }
export function TraceWaterfall({ spans = [], selectedId, onSelect, labelWidth = 200, style }) {
  if (!spans.length) return null;
  const t0 = Math.min(...spans.map((s) => s.startNs));
  const t1 = Math.max(...spans.map((s) => s.endNs));
  const range = Math.max(t1 - t0, 1);
  return <div style={{ fontFamily: 'var(--font-sans)', ...style }}>
    {spans.map((s, i) => {
      const sel = s.id != null && s.id === selectedId;
      const left = ((s.startNs - t0) / range) * 100;
      const width = ((s.endNs - s.startNs) / range) * 100;
      return <div key={s.id || i} onClick={onSelect ? () => onSelect(s) : undefined}
        onMouseEnter={(e) => { if (onSelect) e.currentTarget.style.background = 'var(--bg-sunken)'; }}
        onMouseLeave={(e) => { e.currentTarget.style.background = sel ? 'var(--bg-sunken)' : 'transparent'; }}
        style={{ display: 'grid', gridTemplateColumns: `${labelWidth}px 1fr`, gap: 8, alignItems: 'center', padding: '3px 4px', borderRadius: 'var(--radius-control)', cursor: onSelect ? 'pointer' : 'default', background: sel ? 'var(--bg-sunken)' : 'transparent' }}>
        <div style={{ paddingLeft: (s.depth || 0) * 14, fontSize: 'var(--text-12)', lineHeight: 'var(--lh-12)', whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis', color: s.error ? 'var(--error)' : 'var(--ink)' }}>
          {s.name} {s.service ? <span style={{ color: 'var(--ink-faint)' }}>· {s.service}</span> : null}
        </div>
        <SpanBar startPct={left} widthPct={width} error={s.error} label={fmtMs((s.endNs - s.startNs) / 1e6)} />
      </div>;
    })}
  </div>;
}
