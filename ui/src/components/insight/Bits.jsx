import React from 'react';
import { compactPath, fmtCi, fmtPct, splitToken } from '../../lib/insight.js';
import { Tag } from '../primitives/Tag.jsx';

// The small pieces every insight screen draws with: an interval against a
// base rate, a behaviour path, a row of stat tiles, a before/after diff.
// Everything a server stored — previews, names, prompts, diffs — is rendered
// as a React text node, so it is escaped; nothing here builds HTML.

/** A success rate with its 95% Wilson interval, drawn on a 0–100% track
    with the agent's base rate as an ink hairline. The interval is the band,
    the estimate the tick: the eye reads how wide the evidence is before it
    reads the point. */
export function CiBar({ rate, ci, base, width = 160, height = 14, label = true, style }) {
  const pct = (v) => Math.max(0, Math.min(100, v * 100)) + '%';
  const hasCi = Array.isArray(ci);
  return <span style={{ display: 'inline-flex', flexWrap: 'wrap', alignItems: 'center', gap: '2px 8px', maxWidth: '100%', ...style }}>
    <span role="img" aria-label={`success ${fmtPct(rate)}, 95% interval ${fmtCi(ci)}${typeof base === 'number' ? `, base rate ${fmtPct(base)}` : ''}`}
      style={{ position: 'relative', width, height, flex: 'none' }}>
      <span style={{ position: 'absolute', left: 0, right: 0, top: height / 2 - 1, height: 2, background: 'var(--bg-sunken)', borderRadius: 1 }} />
      {hasCi ? <span style={{
        position: 'absolute', left: pct(ci[0]), width: `calc(${pct(ci[1])} - ${pct(ci[0])})`,
        top: height / 2 - 3, height: 6, background: 'var(--measure-2)', borderRadius: 1.5, minWidth: 2,
      }} /> : null}
      {typeof rate === 'number' ? <span style={{
        position: 'absolute', left: `calc(${pct(rate)} - 1px)`, top: 1, bottom: 1, width: 2,
        background: 'var(--accent)', borderRadius: 1,
      }} /> : null}
      {typeof base === 'number' ? <span title={`base rate ${fmtPct(base)}`} style={{
        position: 'absolute', left: pct(base), top: -2, bottom: -2, width: 1, background: 'var(--ink)',
      }} /> : null}
    </span>
    {label ? <span style={{ fontFamily: 'var(--font-mono)', fontSize: 12, fontVariantNumeric: 'tabular-nums', whiteSpace: 'nowrap' }}>
      <span style={{ color: 'var(--accent)' }}>{fmtPct(rate)}</span>
      {hasCi ? <span style={{ color: 'var(--ink-faint)' }}> {fmtCi(ci)}</span> : null}
    </span> : null}
  </span>;
}

/** One behaviour token, `kind:name`, with the kind faint. */
export function Token({ token, color = 'var(--ink)', strong, title }) {
  const { kind, name } = splitToken(token);
  return <span title={title || token} style={{ fontFamily: 'var(--font-mono)', fontSize: 12, whiteSpace: 'nowrap' }}>
    {kind ? <span style={{ color: 'var(--ink-faint)' }}>{kind}:</span> : null}
    <span style={{ color, fontWeight: strong ? 600 : 400 }}>{name}</span>
  </span>;
}

/** A behaviour path as tokens joined by arrows. The first `matched` tokens
    are solid — the part an agent's map knows — and the rest faint. */
export function PathTokens({ path = [], matched = path.length, accentIndex, wrap = true, max = Infinity, style }) {
  const items = compactPath(path, accentIndex ?? matched, max);
  return <span style={{ display: 'inline-flex', flexWrap: wrap ? 'wrap' : 'nowrap', alignItems: 'center', gap: '2px 6px', minWidth: 0, ...style }}>
    {items.map((item, n) => item.gap ? <React.Fragment key={'gap' + n}>
      <span aria-hidden="true" style={{ color: 'var(--ink-faint)', fontSize: 11 }}>→</span>
      <span title={`${item.gap} more tokens`} style={{ fontFamily: 'var(--font-mono)', fontSize: 11, color: 'var(--ink-faint)', padding: '0 4px', border: '1px dashed var(--hairline)', borderRadius: 'var(--radius-control)' }}>… {item.gap}</span>
    </React.Fragment> : <React.Fragment key={item.index}>
      {n ? <span aria-hidden="true" style={{ color: 'var(--ink-faint)', fontSize: 11 }}>→</span> : null}
      <PathToken token={item.token} i={item.index} matched={matched} accentIndex={accentIndex} />
    </React.Fragment>)}
  </span>;
}

function PathToken({ token, i, matched, accentIndex }) {
  return <span style={{
        padding: '0 4px', borderRadius: 'var(--radius-control)',
        border: `1px ${i < matched ? 'solid' : 'dashed'} ${i === accentIndex ? 'var(--accent)' : 'var(--hairline)'}`,
        background: i === accentIndex ? 'var(--accent-tint)' : i < matched ? 'var(--bg-raised)' : 'transparent',
        opacity: i < matched ? 1 : 0.7,
      }}><Token token={token} /></span>;
}

/** A row of stat tiles, the system's headline strip. */
export function TileRow({ children, columns, style }) {
  const count = columns || React.Children.toArray(children).filter(Boolean).length;
  return <div className="tz-reflow" style={{
    display: 'grid', gridTemplateColumns: `repeat(${count}, minmax(0, 1fr))`,
    background: 'var(--bg-raised)', border: '1px solid var(--hairline)', borderRadius: 'var(--radius-card)',
    overflow: 'hidden', ...style,
  }}>{children}</div>;
}

export function Tile({ label, value, note, tone, title, testId }) {
  return <div title={title} data-testid={testId} style={{
    padding: '10px 14px', borderRight: '1px solid var(--hairline)', borderBottom: '1px solid var(--hairline)',
    minWidth: 0, marginBottom: -1,
  }}>
    <div style={{ fontSize: 11, color: 'var(--ink-muted)', marginBottom: 3, whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis' }}>{label}</div>
    <div style={{
      fontFamily: 'var(--font-mono)', fontSize: 16, fontVariantNumeric: 'tabular-nums',
      color: tone || 'var(--accent)', whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis',
    }}>{value}{note ? <span style={{ fontSize: 11, color: 'var(--ink-muted)', marginLeft: 6 }}>{note}</span> : null}</div>
  </div>;
}

/** The first differing stretch of two texts: what was there, struck and
    faint, and what replaced it, on the accent tint — a timestamp in a
    system prompt reads at a glance. `context` is the text just before. */
export function DiffPair({ diff, context, style }) {
  if (!diff) return <span style={{ fontSize: 12, color: 'var(--ink-faint)' }}>the text was not kept</span>;
  return <span style={{ fontFamily: 'var(--font-mono)', fontSize: 12, lineHeight: '18px', overflowWrap: 'anywhere', ...style }}>
    {context ? <span style={{ color: 'var(--ink-faint)' }}>…{context}</span> : null}
    <span style={{ textDecoration: 'line-through', color: 'var(--ink-muted)', background: 'var(--bg-sunken)', padding: '0 2px', borderRadius: 2 }}>{diff.before || '∅'}</span>
    <span aria-hidden="true" style={{ color: 'var(--ink-faint)', margin: '0 5px' }}>→</span>
    <span style={{ color: 'var(--ink)', background: 'var(--accent-tint)', padding: '0 2px', borderRadius: 2 }}>{diff.after || '∅'}</span>
    {typeof diff.offset === 'number' ? <span style={{ color: 'var(--ink-faint)', marginLeft: 8, fontSize: 11 }}>@ byte {diff.offset}</span> : null}
  </span>;
}

const OUTCOME_TONE = { success: 'ok', failure: 'error', abandoned: 'warn', unknown: 'neutral' };

/** How a run ended, as the session-outcome rule reads it. */
export function OutcomeTag({ outcome }) {
  return <Tag status={OUTCOME_TONE[outcome] || 'neutral'} mono>{outcome || 'unknown'}</Tag>;
}

/** A floating reading box for a chart, positioned inside a relative
    container. It enhances the chart; the same figures are in a panel or a
    table beside it. */
export function Tooltip({ x, y, children, containerWidth, width = 280 }) {
  const left = containerWidth && x + width + 16 > containerWidth ? Math.max(4, x - width - 12) : x + 14;
  return <div role="tooltip" style={{
    position: 'absolute', left, top: y + 12, width, zIndex: 30, pointerEvents: 'none',
    background: 'var(--bg-raised)', border: '1px solid var(--hairline)', borderRadius: 'var(--radius-card)',
    padding: '9px 11px', fontSize: 12, lineHeight: '18px', color: 'var(--ink)',
    boxShadow: '0 1px 0 var(--hairline)',
  }}>{children}</div>;
}

/** A label/value line inside a tooltip or panel. */
export function Kv({ k, children, title }) {
  return <div title={title} style={{ display: 'flex', gap: 10, justifyContent: 'space-between', minWidth: 0 }}>
    <span style={{ color: 'var(--ink-muted)', whiteSpace: 'nowrap' }}>{k}</span>
    <span style={{ fontFamily: 'var(--font-mono)', fontVariantNumeric: 'tabular-nums', textAlign: 'right', minWidth: 0, overflowWrap: 'anywhere' }}>{children}</span>
  </div>;
}

/** A legend swatch. */
export function Swatch({ color, hatch, outline, size = 10, style }) {
  return <span aria-hidden="true" style={{
    display: 'inline-block', width: size, height: size, borderRadius: 2, flex: 'none',
    background: hatch ? `repeating-linear-gradient(135deg, ${color} 0 2px, var(--bg-raised) 2px 4px)` : color,
    border: outline ? '1px solid var(--hairline)' : 'none', ...style,
  }} />;
}

/** A bounded-work caveat line, in the system's warning ink. */
export function Caveat({ children, testId }) {
  return <div data-testid={testId} role="note" style={{ fontSize: 12, color: 'var(--warn)', lineHeight: '18px' }}>{children}</div>;
}
