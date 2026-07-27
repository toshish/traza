import React from 'react';
import { fmtClockNs, fmtDurationNs } from '../../lib/format.js';

// The drawn marks. One motif throughout: a bar is a span, a stack is a trace,
// a row of bars is a distribution. Everything is divs rather than SVG — these
// are dense, they restyle with tokens, and a 400-bar sparkline as divs is
// cheaper to lay out than the equivalent SVG path is to parse.

/** Scales a value into a percentage height, floored so a non-zero bucket is
    never invisible. A bar that rounds to nothing reads as "no data", which is
    a different claim from "a little data". */
function heightOf(value, max) {
  if (!max || !value) return value ? '2px' : '1px';
  return Math.max(2, (value / max) * 100) + '%';
}

/** A compact bar sparkline. `values` are raw; `accent` picks which bars carry
    the measured hue against the ink series. */
export function Sparkbar({ values = [], height = 22, color = 'var(--measure-3)', highlight, gap = 1, style }) {
  const max = Math.max(...values, 0);
  return <div style={{ display: 'flex', alignItems: 'flex-end', gap, height, ...style }}>
    {values.map((value, i) => <div key={i} style={{
      flex: 1, height: heightOf(value, max), minHeight: 1, borderRadius: 1,
      background: highlight && highlight(value, i) ? 'var(--accent)' : color,
    }} />)}
  </div>;
}

/** A stacked sparkline: errors above, everything else below. Used wherever
    the question is "how much of this volume was failing". */
export function StackedSparkbar({ buckets = [], height = 38, style }) {
  const max = Math.max(...buckets.map((b) => b.spans || 0), 0);
  return <div style={{ display: 'flex', alignItems: 'flex-end', gap: 2, height, ...style }}>
    {buckets.map((bucket, i) => {
      const total = bucket.spans || 0;
      const errors = bucket.errors || 0;
      return <div key={i} style={{
        flex: 1, display: 'flex', flexDirection: 'column', justifyContent: 'flex-end',
        height: '100%', gap: 1,
      }}>
        <div style={{ height: heightOf(errors, max), background: 'var(--error)', borderRadius: 1 }} />
        <div style={{ height: heightOf(total - errors, max), background: 'var(--series-4)', borderRadius: 1 }} />
      </div>;
    })}
  </div>;
}

/** The volume chart with a drag-selectable window.

    Dragging emits a nanosecond range, which is what makes "zoom into the
    spike" one gesture instead of typing two timestamps into two boxes. */
export function VolumeBrush({ buckets = [], bucketNs, sinceNs, selection, onSelect, height = 46, style }) {
  const ref = React.useRef(null);
  const [drag, setDrag] = React.useState(null);
  const max = Math.max(...buckets.map((b) => b.spans || 0), 0);

  const fractionAt = (clientX) => {
    const box = ref.current?.getBoundingClientRect();
    if (!box || !box.width) return 0;
    return Math.min(1, Math.max(0, (clientX - box.left) / box.width));
  };

  const finish = (from, to) => {
    if (!onSelect || from == null || to == null) return;
    const [lo, hi] = from <= to ? [from, to] : [to, from];
    // A click is not a selection. Below a bucket's width there is nothing to
    // zoom into, and treating a stray click as a range is how a brush becomes
    // a trap you have to undo.
    if (Math.abs(hi - lo) < 0.01) return;
    const span = buckets.length * bucketNs;
    onSelect({
      sinceNs: Math.round(sinceNs + lo * span),
      untilNs: Math.round(sinceNs + hi * span),
    });
  };

  const selected = selection && bucketNs && buckets.length ? {
    left: ((selection.sinceNs - sinceNs) / (buckets.length * bucketNs)) * 100,
    right: 100 - ((selection.untilNs - sinceNs) / (buckets.length * bucketNs)) * 100,
  } : null;

  return <div ref={ref}
    onMouseDown={(e) => { const at = fractionAt(e.clientX); setDrag({ from: at, to: at }); }}
    onMouseMove={(e) => { if (drag) setDrag((d) => ({ ...d, to: fractionAt(e.clientX) })); }}
    onMouseUp={(e) => { if (drag) { finish(drag.from, fractionAt(e.clientX)); setDrag(null); } }}
    onMouseLeave={() => setDrag(null)}
    style={{
      position: 'relative', display: 'flex', alignItems: 'flex-end', gap: 1, height,
      borderBottom: '1px solid var(--ink)', cursor: onSelect ? 'ew-resize' : 'default',
      userSelect: 'none', ...style,
    }}>
    {buckets.map((bucket, i) => <div key={i} style={{
      flex: 1, height: heightOf(bucket.spans, max), minHeight: 1, borderRadius: 1,
      background: bucket.errors ? 'var(--measure-5)' : 'var(--measure-3)',
    }} />)}
    {drag && Math.abs(drag.to - drag.from) > 0.005 ? <div style={{
      position: 'absolute', left: Math.min(drag.from, drag.to) * 100 + '%',
      right: (1 - Math.max(drag.from, drag.to)) * 100 + '%', top: -4, bottom: 0,
      borderLeft: '1px solid var(--accent)', borderRight: '1px solid var(--accent)',
      background: 'rgba(198,93,59,0.06)', pointerEvents: 'none',
    }} /> : selected ? <div style={{
      position: 'absolute', left: selected.left + '%', right: selected.right + '%', top: -4, bottom: 0,
      borderLeft: '1px solid var(--accent)', borderRight: '1px solid var(--accent)',
      background: 'rgba(198,93,59,0.06)', pointerEvents: 'none',
    }} /> : null}
  </div>;
}

/** A duration distribution with percentile marks.

    The system's one drawn convention for percentiles: a solid ink hairline
    for the median, dashed for the tail, labels in mono above the line. */
export function Distribution({ buckets = [], p50Ns, p95Ns, p99Ns, height = 150, style }) {
  if (!buckets.length) return null;
  const max = Math.max(...buckets.map((b) => b.count), 0);
  // Position marks on the log-spaced bucket axis rather than a linear one:
  // durations span microseconds to minutes, and a linear axis puts every
  // interesting bucket in the first pixel.
  const lo = Math.log2(Math.max(1, buckets[0].upper_ns));
  const hi = Math.log2(Math.max(2, buckets[buckets.length - 1].upper_ns));
  const at = (ns) => (!ns || hi <= lo) ? null : ((Math.log2(Math.max(1, ns)) - lo) / (hi - lo)) * 100;

  const mark = (ns, label, dashed) => {
    const left = at(ns);
    if (left == null) return null;
    return <React.Fragment key={label}>
      <div style={{
        position: 'absolute', left: left + '%', top: 0, bottom: 0, width: 1,
        background: dashed ? 'repeating-linear-gradient(var(--ink) 0 3px,transparent 3px 6px)' : 'var(--ink)',
      }} />
      <div style={{
        position: 'absolute', left: `min(${left}%, calc(100% - 62px))`, top: -17,
        fontFamily: 'var(--font-mono)', fontSize: 11, color: 'var(--ink-muted)', whiteSpace: 'nowrap',
      }}>{label} {fmtDurationNs(ns)}</div>
    </React.Fragment>;
  };

  return <div style={{ paddingTop: 18, ...style }}>
    <div style={{
      position: 'relative', display: 'flex', alignItems: 'flex-end', gap: 1, height,
      borderBottom: '1px solid var(--hairline)',
    }}>
      {buckets.map((bucket, i) => <div key={i} title={`≤ ${fmtDurationNs(bucket.upper_ns)} · ${bucket.count}`}
        style={{
          flex: 1, height: heightOf(bucket.count, max), minHeight: 1,
          background: 'var(--measure-3)', borderRadius: '1.5px 1.5px 0 0',
        }} />)}
      {mark(p50Ns, 'p50', false)}
      {mark(p95Ns, 'p95', true)}
      {mark(p99Ns, 'p99', true)}
    </div>
    <div style={{
      display: 'flex', justifyContent: 'space-between', marginTop: 5,
      fontFamily: 'var(--font-mono)', fontSize: 11, color: 'var(--ink-faint)',
    }}>
      <span>{fmtDurationNs(buckets[0].upper_ns)}</span>
      <span>{fmtDurationNs(buckets[buckets.length - 1].upper_ns)}</span>
    </div>
  </div>;
}

/** A time axis. Every waterfall gets one: bars positioned proportionally with
    no ruler are a picture of a trace, not a reading of it. */
export function TimeRuler({ startNs, endNs, ticks = 6, style }) {
  const span = Math.max(1, endNs - startNs);
  const marks = Array.from({ length: ticks + 1 }, (_, i) => i / ticks);
  return <div style={{ position: 'relative', height: 16, ...style }}>
    {marks.map((fraction, i) => <div key={i} style={{
      position: 'absolute', left: fraction * 100 + '%', top: 0, bottom: 0,
      borderLeft: '1px solid var(--hairline)',
    }}>
      <span style={{
        position: 'absolute', top: 1, left: i === ticks ? undefined : 3, right: i === ticks ? 3 : undefined,
        fontFamily: 'var(--font-mono)', fontSize: 11, color: 'var(--ink-faint)',
        whiteSpace: 'nowrap', fontVariantNumeric: 'tabular-nums',
      }}>{fmtDurationNs(span * fraction)}</span>
    </div>)}
  </div>;
}

/** Horizontal axis labels for a time series. */
export function TimeAxis({ sinceNs, untilNs, ticks = 4, style }) {
  const marks = Array.from({ length: ticks + 1 }, (_, i) => i / ticks);
  return <div style={{
    display: 'flex', justifyContent: 'space-between', marginTop: 4,
    fontFamily: 'var(--font-mono)', fontSize: 11, color: 'var(--ink-faint)', ...style,
  }}>
    {marks.map((fraction, i) => <span key={i}>{fmtClockNs(sinceNs + (untilNs - sinceNs) * fraction)}</span>)}
  </div>;
}

/** A proportion drawn as a single split bar — "12 of 340 segments read". */
export function ShareBar({ part, whole, width = 150, height = 9, style }) {
  const fraction = whole ? Math.max(0.5, (part / whole) * 100) : 0;
  return <div style={{ display: 'flex', gap: 1, height, width, alignItems: 'stretch', ...style }}>
    <div style={{ width: fraction + '%', background: 'var(--accent)', borderRadius: 1 }} />
    <div style={{ flex: 1, background: 'var(--bg-sunken)', borderRadius: 1 }} />
  </div>;
}

/** A grouped bar chart over the categorical ink ladder, accent for the focus
    series. Used by Analytics for any measure over any grouping. */
export function CategoryBars({ rows = [], height = 150, valueOf, labelOf, format, style }) {
  const max = Math.max(...rows.map(valueOf), 0);
  return <div style={{ ...style }}>
    <div style={{ display: 'flex', alignItems: 'flex-end', gap: 6, height }}>
      {rows.map((row, i) => <div key={i} title={`${labelOf(row)} · ${format(valueOf(row))}`} style={{
        flex: 1, display: 'flex', flexDirection: 'column', justifyContent: 'flex-end', height: '100%', minWidth: 0,
      }}>
        <div style={{
          height: heightOf(valueOf(row), max), minHeight: 2,
          background: i === 0 ? 'var(--accent)' : `var(--measure-${Math.max(1, 4 - Math.floor(i / 3))})`,
          borderRadius: '1.5px 1.5px 0 0',
        }} />
      </div>)}
    </div>
    <div style={{ display: 'flex', gap: 6, marginTop: 5 }}>
      {rows.map((row, i) => <div key={i} style={{
        flex: 1, minWidth: 0, fontSize: 11, color: 'var(--ink-faint)', textAlign: 'center',
        overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
      }}>{labelOf(row)}</div>)}
    </div>
  </div>;
}

/** A line over time, drawn as an SVG polyline — a line is the one mark where
    the path really is cheaper than the divs. */
export function TimeLine({ buckets = [], valueOf, height = 120, style }) {
  if (buckets.length < 2) return null;
  const values = buckets.map(valueOf);
  const max = Math.max(...values, 0) || 1;
  const points = values
    .map((value, i) => `${(i / (values.length - 1)) * 100},${100 - (value / max) * 100}`)
    .join(' ');
  return <svg viewBox="0 0 100 100" preserveAspectRatio="none" aria-hidden="true"
    style={{ width: '100%', height, display: 'block', ...style }}>
    <polyline points={points} fill="none" stroke="var(--accent)" strokeWidth="1"
      vectorEffect="non-scaling-stroke" />
  </svg>;
}
