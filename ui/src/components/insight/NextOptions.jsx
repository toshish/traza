import React from 'react';
import { fmtNum } from '../../lib/format.js';
import { fmtPct, rateColor } from '../../lib/insight.js';
import { Mono } from '../primitives/Chrome.jsx';
import { Tag } from '../primitives/Tag.jsx';
import { CiBar, Token } from './Bits.jsx';

/** What earlier runs did next from where this run is. */
export function NextOptions({ foresight }) {
  const f = foresight;
  if (f.divergence) {
    return <div style={{ display: 'grid', gap: 6, fontSize: 12 }}>
      <div style={{ color: 'var(--ink-muted)' }}>It took <Token token={f.divergence.took} color="var(--accent-hover)" strong /> where no earlier run did. The runs that got here went on with:</div>
      <div style={{ display: 'flex', gap: 6, flexWrap: 'wrap' }}>{f.divergence.common_next.map((t) => <span key={t} style={{
        border: `1px solid ${t === f.recommended_next ? 'var(--accent)' : 'var(--hairline)'}`, borderRadius: 'var(--radius-control)', padding: '0 4px',
      }}><Token token={t} /></span>)}</div>
      {f.next && f.next.length ? <NextTable next={f.next} recommended={f.recommended_next} base={f.base_rate} /> : null}
    </div>;
  }
  if (!f.next || !f.next.length) {
    return <div style={{ fontSize: 12, color: 'var(--ink-muted)' }}>{f.last_open ? 'No option from here has three or more runs.' : 'The run has ended; it is judged at its own node.'}</div>;
  }
  return <NextTable next={f.next} recommended={f.recommended_next} base={f.base_rate} />;
}

function NextTable({ next, recommended, base }) {
  return <div style={{ display: 'grid', gap: 5 }} data-testid="next-options">
    <div style={{ fontSize: 11, textTransform: 'uppercase', letterSpacing: 'var(--tracking-caps)', color: 'var(--ink-muted)', fontWeight: 500 }}>what earlier runs did next</div>
    {next.map((n) => <div key={n.token + (n.repeat ? ':r' : '')} style={{ display: 'grid', gridTemplateColumns: 'minmax(0, 1fr) auto', gap: 8, alignItems: 'center' }}>
      <span style={{ display: 'flex', alignItems: 'center', gap: 6, minWidth: 0 }}>
        <span style={{ width: 8, height: 8, borderRadius: 2, background: rateColor(n.success_rate, base), flex: 'none' }} />
        <Token token={n.token} strong={n.token === recommended && !n.repeat} />
        {n.repeat ? <span style={{ fontSize: 11, color: 'var(--ink-faint)' }}>again</span> : null}
        {n.token === recommended && !n.repeat ? <Tag status="accent" mono>recommended</Tag> : null}
      </span>
      <span style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
        <CiBar rate={n.success_rate} ci={n.ci95} base={base} width={90} label={false} />
        <Mono size={12} color="var(--accent)">{fmtPct(n.success_rate, 0)}</Mono>
        <Mono size={11} color="var(--ink-faint)">{fmtNum(n.visits)}</Mono>
      </span>
    </div>)}
  </div>;
}
