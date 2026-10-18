import React from 'react';
import { fmtNum } from '../../lib/format.js';
import { chunkClass, chunkFill, classTotals, provenanceGroups, provenanceKey } from '../../lib/insight.js';
import { Kv, Swatch, Tooltip } from './Bits.jsx';

// One model call's context window as one bar: every chunk a segment as wide
// as its estimated tokens, in the order the model read them. Fill says what
// kind of content it is (ink, darkest for instructions); hatching marks a
// stale tool result; a warning tick marks a repeat. Hovering a chunk lights
// up, in the accent, every chunk the same step introduced — provenance is
// the question this bar answers.

const STALE_HATCH = (fill) => `repeating-linear-gradient(135deg, ${fill} 0 3px, var(--bg-raised) 3px 5px)`;

/** Where a chunk came from, in words. */
export function introducedLabel(chunk) {
  const by = chunk && chunk.introduced_by;
  if (!by || typeof by.step_index !== 'number') return 'new in this call';
  return `step ${by.step_index} · ${by.kind}${by.name ? ' ' + by.name : ''}`;
}

export function ContextBar({ chunks, focusKey, onFocus, onPick, height = 30 }) {
  const wrap = React.useRef(null);
  const [hover, setHover] = React.useState(null);
  const total = chunks.reduce((sum, c) => sum + (c.est_tokens || 0), 0);
  const hovered = hover ? chunks.find((c) => c.index === hover.index) : null;
  const lit = hovered ? provenanceKey(hovered) : focusKey;

  const move = (event, chunk) => {
    const box = wrap.current?.getBoundingClientRect();
    if (!box) return;
    setHover({ index: chunk.index, x: event.clientX - box.left, y: event.clientY - box.top, width: box.width });
  };

  return <div ref={wrap} style={{ position: 'relative' }} onMouseLeave={() => { setHover(null); }}>
    <div role="list" aria-label={`Context window: ${chunks.length} chunks, about ${fmtNum(total)} estimated tokens`}
      style={{ display: 'flex', gap: 1, height, alignItems: 'stretch', borderRadius: 2, overflow: 'hidden' }}>
      {chunks.map((chunk) => {
        const share = total ? (chunk.est_tokens || 0) / total : 1 / chunks.length;
        const focused = lit && provenanceKey(chunk) === lit;
        const fill = focused ? 'var(--accent)' : chunkFill(chunk);
        return <div key={chunk.index} role="listitem" data-testid="context-segment" data-stale={chunk.stale ? 'true' : 'false'}
          aria-label={`chunk ${chunk.index}, ${chunk.kind}, ${chunk.est_tokens} tokens, ${introducedLabel(chunk)}${chunk.stale ? ', stale' : ''}${chunk.duplicate_of != null ? ', duplicate of ' + chunk.duplicate_of : ''}`}
          tabIndex={0}
          onMouseMove={(e) => move(e, chunk)}
          onFocus={() => onFocus && onFocus(provenanceKey(chunk))}
          onBlur={() => onFocus && onFocus(null)}
          onClick={() => onPick && onPick(chunk)}
          onKeyDown={(e) => { if (e.key === 'Enter') onPick && onPick(chunk); }}
          style={{
            flex: `${Math.max(share, 0.0005)} 1 0`, minWidth: 2, position: 'relative', cursor: 'pointer',
            background: chunk.stale ? STALE_HATCH(fill) : fill,
            // Paper-filled kinds (definitions, other) carry an inset ink
            // edge so they read as present, not as a gap.
            boxShadow: !focused && (chunkClass(chunk) === 'definitions' || chunkClass(chunk) === 'other') ? 'inset 0 0 0 1px var(--series-3)' : 'none',
          }}>
          {chunk.duplicate_of != null ? <span style={{ position: 'absolute', left: 0, right: 0, top: 0, height: 3, background: 'var(--warn)' }} /> : null}
        </div>;
      })}
    </div>
    <div style={{ display: 'flex', justifyContent: 'space-between', marginTop: 4, fontFamily: 'var(--font-mono)', fontSize: 11, color: 'var(--ink-faint)' }}>
      <span>first read</span>
      <span>~{fmtNum(total)} est. tokens</span>
      <span>last read</span>
    </div>
    {hovered ? <Tooltip x={hover.x} y={hover.y} containerWidth={hover.width} width={300}>
      <div style={{ display: 'grid', gap: 3 }}>
        <div style={{ fontFamily: 'var(--font-mono)' }}>#{hovered.index} · {hovered.role || 'no role'} · {hovered.kind}{hovered.name ? ' · ' + hovered.name : ''}</div>
        <Kv k="est. tokens">{fmtNum(hovered.est_tokens)}</Kv>
        <Kv k="introduced by">{introducedLabel(hovered)}</Kv>
        {typeof hovered.age_steps === 'number' ? <Kv k="age">{hovered.age_steps} steps</Kv> : null}
        {hovered.stale ? <Kv k="stale">never drawn on since</Kv> : null}
        {hovered.duplicate_of != null ? <Kv k="repeat of">chunk {hovered.duplicate_of}</Kv> : null}
        <div style={{ marginTop: 3, fontFamily: 'var(--font-mono)', fontSize: 11, color: 'var(--ink-muted)', whiteSpace: 'pre-wrap', overflowWrap: 'anywhere', maxHeight: 90, overflow: 'hidden' }}>{hovered.preview}</div>
      </div>
    </Tooltip> : null}
  </div>;
}

/** What the fills mean, with the tokens of each, and the two marks. */
export function ContextLegend({ chunks }) {
  const totals = classTotals(chunks);
  const stale = chunks.filter((c) => c.stale).reduce((s, c) => s + (c.est_tokens || 0), 0);
  const dup = chunks.some((c) => c.duplicate_of != null);
  return <div style={{ display: 'flex', flexWrap: 'wrap', gap: '4px 14px', fontSize: 12, color: 'var(--ink-muted)' }}>
    {totals.map((c) => <span key={c.id} style={{ display: 'inline-flex', alignItems: 'center', gap: 5 }}>
      <Swatch color={c.fill} style={c.id === 'definitions' || c.id === 'other' ? { boxShadow: 'inset 0 0 0 1px var(--series-3)' } : undefined} />{c.label}
      <span style={{ fontFamily: 'var(--font-mono)', fontSize: 11, color: 'var(--ink-faint)' }}>{fmtNum(c.tokens)}</span>
    </span>)}
    {stale ? <span style={{ display: 'inline-flex', alignItems: 'center', gap: 5 }}><Swatch color="var(--series-2)" hatch />stale
      <span style={{ fontFamily: 'var(--font-mono)', fontSize: 11, color: 'var(--ink-faint)' }}>{fmtNum(stale)}</span></span> : null}
    {dup ? <span style={{ display: 'inline-flex', alignItems: 'center', gap: 5 }}><Swatch color="var(--warn)" style={{ height: 3, alignSelf: 'flex-start', marginTop: 6 }} />repeat</span> : null}
    <span style={{ display: 'inline-flex', alignItems: 'center', gap: 5 }}><Swatch color="var(--accent)" />the step in focus</span>
  </div>;
}

/** The context totalled by the step that introduced it: a row of chips,
    most tokens first. Hovering one lights its chunks; clicking narrows the
    chunk list to them. */
export function ProvenanceChips({ chunks, focusKey, onFocus, filterKey, onFilter, limit = 8 }) {
  const groups = provenanceGroups(chunks);
  const [all, setAll] = React.useState(false);
  const shown = all ? groups : groups.slice(0, limit);
  return <div style={{ display: 'flex', flexWrap: 'wrap', gap: 6, alignItems: 'center' }} data-testid="provenance">
    <span style={{ fontSize: 12, color: 'var(--ink-muted)', marginRight: 2 }}>introduced by</span>
    {shown.map((g) => {
      const active = filterKey === g.key;
      const focused = focusKey === g.key;
      return <button key={g.key} type="button" className="tz-plain-button"
        onMouseEnter={() => onFocus(g.key)} onMouseLeave={() => onFocus(null)} onFocus={() => onFocus(g.key)} onBlur={() => onFocus(null)}
        onClick={() => onFilter(active ? null : g.key)} aria-pressed={active}
        title={`${g.chunks} chunk${g.chunks === 1 ? '' : 's'}${g.stale ? `, ${g.stale} stale` : ''}${g.via === 'tool_call_id' ? ' — matched by tool-call id: the model saw something other than what the tool returned' : ''}`}
        style={{
          display: 'inline-flex', alignItems: 'baseline', gap: 6, padding: '2px 8px', cursor: 'pointer',
          border: `1px solid ${active || focused ? 'var(--accent)' : 'var(--hairline)'}`, borderRadius: 'var(--radius-control)',
          background: active ? 'var(--accent-tint)' : 'var(--bg-raised)', fontSize: 12,
        }}>
        <span style={{ fontFamily: 'var(--font-mono)', color: 'var(--ink)' }}>{g.step == null ? 'this call' : `step ${g.step}`}</span>
        {g.kind ? <span style={{ fontFamily: 'var(--font-mono)', fontSize: 11, color: 'var(--ink-faint)' }}>{g.kind}{g.name ? ' ' + g.name : ''}</span> : null}
        <span style={{ fontFamily: 'var(--font-mono)', fontSize: 11, color: 'var(--accent)' }}>{Math.round(g.share * 100)}%</span>
      </button>;
    })}
    {groups.length > limit ? <button type="button" className="tz-plain-button" onClick={() => setAll((v) => !v)}
      style={{ fontSize: 12, color: 'var(--ink-muted)', cursor: 'pointer', textDecoration: 'underline' }}>
      {all ? 'fewer' : `${groups.length - limit} more`}
    </button> : null}
  </div>;
}

