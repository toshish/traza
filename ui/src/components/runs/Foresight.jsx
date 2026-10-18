import React from 'react';
import { Tag } from '../primitives/Tag.jsx';
import { FORESIGHT, fmtInterval, fmtPct, foresightMarks, foresightSentence } from '../../lib/live.js';

// A run's foresight, drawn: how earlier runs of its agent that took the same
// path ended. The rate is a measurement, so it is the accent; its 95%
// interval is the same hue, lighter; the agent's base rate is an ink tick to
// compare against. Every number comes from the server's counts — nothing
// here estimates anything.

/** The verdict as a chip: on track, at risk, likely failure, novel path, or
    no history. */
export function ForesightChip({ foresight, style }) {
  if (!foresight || !foresight.verdict) {
    return <span title="The server holds no whole path for this run yet" style={{ fontSize: 11, color: 'var(--ink-faint)', ...style }}>no foresight</span>;
  }
  const known = FORESIGHT[foresight.verdict] || { label: foresight.verdict.replace(/_/g, ' '), tag: 'neutral' };
  if (foresight.verdict === 'insufficient_history') {
    return <span title={foresightSentence(foresight)} style={{ fontSize: 11, color: 'var(--ink-faint)', whiteSpace: 'nowrap', ...style }}>{known.label}</span>;
  }
  return <Tag status={known.tag} mono style={{
    ...(foresight.verdict === 'novel' ? { color: 'var(--ink)', border: '1px dashed var(--ink-faint)' } : {}),
    ...style,
  }}>{known.label}</Tag>;
}

/** The rate as it should be read: this run's path's rate, or — for a run
    on a novel path — the rate of the path before it left it, which is not
    this run's odds and is shown muted and labelled `before`. */
function Rate({ foresight, marks, faded }) {
  const before = foresight.verdict === 'novel';
  return <span title={before ? 'the success rate of the path before this run left it' : undefined}
    style={{ color: faded || before ? 'var(--ink-muted)' : 'var(--accent)', fontWeight: 500 }}>
    {before ? <span style={{ fontWeight: 400 }}>before </span> : null}{fmtPct(marks.p)}
  </span>;
}

/** The rate on a 0–100% track, with its interval, the base rate and the
    number of runs behind it.

    `compact` is the phone card: the track and one line of figures, the run
    count always among them — a touch screen shows no tooltip. */
export function ForesightBar({ foresight, compact = false, width = '100%' }) {
  const marks = foresightMarks(foresight);
  const title = foresightSentence(foresight);
  if (!marks || marks.p == null) {
    return <div title={title} style={{ width, display: 'grid', gap: 3 }}>
      <Track width={width} />
      {!compact ? <div style={{ fontSize: 11, color: 'var(--ink-faint)' }}>{title}</div> : null}
    </div>;
  }
  const faded = foresight.verdict === 'insufficient_history' || foresight.verdict === 'novel';
  return <div title={title} role="img" aria-label={title} style={{ width, display: 'grid', gap: 3, minWidth: 0 }}>
    <Track width={width} marks={marks} faded={faded} />
    <div style={{
      display: 'flex', alignItems: 'baseline', gap: 6, fontFamily: 'var(--font-mono)', fontSize: 11,
      fontVariantNumeric: 'tabular-nums', whiteSpace: 'nowrap', overflow: 'hidden',
    }}>
      <Rate foresight={foresight} marks={marks} faded={foresight.verdict === 'insufficient_history'} />
      {marks.lo != null && marks.hi != null ? <span style={{ color: 'var(--ink-muted)' }}>{fmtInterval(foresight.ci95)}</span> : null}
      <span style={{ color: 'var(--ink-muted)' }}>n {marks.support}</span>
      {marks.base != null ? <span style={{ color: 'var(--ink-faint)' }}>base {fmtPct(marks.base)}</span> : null}
    </div>
  </div>;
}

/** The table cell: the verdict, the rate, its interval and the runs behind
    it, the track under them; the base rate is drawn, and spelled out in the
    title. The figures wrap under the chip when the column is narrow rather
    than being cut. A run without enough history says how much it has, and
    draws nothing: an interval over three runs is a picture of noise. */
export function ForesightCell({ foresight }) {
  const marks = foresightMarks(foresight);
  const title = foresightSentence(foresight);
  if (!foresight || !marks || foresight.verdict === 'insufficient_history' || marks.p == null) {
    return <div title={title} style={{ display: 'flex', alignItems: 'baseline', gap: 6, minWidth: 0, whiteSpace: 'nowrap' }}>
      <ForesightChip foresight={foresight} />
      {foresight && foresight.verdict === 'insufficient_history'
        ? <span style={{ fontFamily: 'var(--font-mono)', fontSize: 11, color: 'var(--ink-faint)', fontVariantNumeric: 'tabular-nums' }}>n {marks ? marks.support : 0}/30</span> : null}
    </div>;
  }
  const novel = foresight.verdict === 'novel';
  return <div title={title} role="img" aria-label={title} style={{ display: 'grid', gap: 3, minWidth: 0 }}>
    <div style={{ display: 'flex', alignItems: 'center', columnGap: 6, rowGap: 2, minWidth: 0, flexWrap: 'wrap' }}>
      <ForesightChip foresight={foresight} />
      <span style={{ marginLeft: 'auto', fontFamily: 'var(--font-mono)', fontSize: 11, fontVariantNumeric: 'tabular-nums', display: 'inline-flex', gap: 5, whiteSpace: 'nowrap' }}>
        <Rate foresight={foresight} marks={marks} />
        {marks.lo != null && marks.hi != null ? <span style={{ color: 'var(--ink-faint)' }}>{fmtInterval(foresight.ci95)}</span> : null}
        <span style={{ color: 'var(--ink-faint)' }}>n {marks.support}</span>
      </span>
    </div>
    <Track width="100%" marks={marks} faded={novel} />
  </div>;
}

function Track({ width, marks, faded }) {
  const pct = (value) => `${(value * 100).toFixed(2)}%`;
  return <div aria-hidden="true" style={{ position: 'relative', height: 10, width }}>
    <div style={{ position: 'absolute', left: 0, right: 0, top: 4, height: 2, background: 'var(--bg-sunken)', borderRadius: 1 }} />
    {marks && marks.lo != null && marks.hi != null ? <div style={{
      position: 'absolute', top: 2, height: 6, left: pct(marks.lo), width: pct(Math.max(0.004, marks.hi - marks.lo)),
      background: faded ? 'var(--bg-sunken)' : 'var(--measure-2)', borderRadius: 1.5,
      border: faded ? '1px dashed var(--ink-faint)' : '1px solid transparent', boxSizing: 'border-box',
    }} /> : null}
    {marks && marks.base != null ? <div title="the agent's base rate" style={{
      position: 'absolute', top: 0, height: 10, width: 1, left: pct(marks.base), background: 'var(--ink-muted)',
    }} /> : null}
    {marks && marks.p != null ? <div style={{
      position: 'absolute', top: 1, height: 8, width: 3, marginLeft: -1.5, left: pct(marks.p),
      background: faded ? 'var(--ink-faint)' : 'var(--accent)', borderRadius: 1,
    }} /> : null}
  </div>;
}
