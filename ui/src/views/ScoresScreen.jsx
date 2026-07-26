import React from 'react';
import { api } from '../lib/api.js';
import { useRead, useKeys } from '../lib/route.js';
import { RANGES, windowOf } from '../lib/query.js';
import { fmtAgo, fmtNum, fmtPercent } from '../lib/format.js';
import { Card, Chip, Eyebrow, ErrorState, EmptyState, Kbd, LoadingBar } from '../components/primitives/Chrome.jsx';

// Annotations read across traces. Until now `GET /v1/annotations` required a
// trace_id, so an eval run's results could only be read one trace at a time —
// which is to say, not read at all. A score is produced per trace but it is
// only meaningful as a population.

/** Numeric values get a distribution; everything else gets a tally. Mixing
    them into one chart would average booleans with 0-to-1 scores. */
function summarize(annotations) {
  const byName = new Map();
  for (const annotation of annotations) {
    if (!byName.has(annotation.name)) {
      byName.set(annotation.name, { name: annotation.name, values: [], human: 0, evaluated: 0, other: 0 });
    }
    const entry = byName.get(annotation.name);
    entry.values.push(annotation.value);
    if (annotation.source?.startsWith('human:')) entry.human += 1;
    else if (annotation.source?.startsWith('eval:')) entry.evaluated += 1;
    else entry.other += 1;
  }
  return [...byName.values()].map((entry) => {
    const numbers = entry.values.filter((v) => typeof v === 'number' && Number.isFinite(v));
    const numeric = numbers.length === entry.values.length && numbers.length > 0;
    const sorted = [...numbers].sort((a, b) => a - b);
    return {
      ...entry,
      count: entry.values.length,
      numeric,
      mean: numbers.length ? numbers.reduce((t, v) => t + v, 0) / numbers.length : null,
      min: sorted[0] ?? null,
      max: sorted[sorted.length - 1] ?? null,
      p50: sorted.length ? sorted[Math.floor(sorted.length / 2)] : null,
      // Ten even buckets across the observed span — score scales are not
      // always 0..1, and assuming they are draws an empty chart for a 1..5.
      histogram: numeric ? bucketize(sorted, 10) : null,
      tally: numeric ? null : tally(entry.values),
    };
  }).sort((a, b) => b.count - a.count);
}

function bucketize(sorted, count) {
  if (!sorted.length) return [];
  const lo = sorted[0];
  const hi = sorted[sorted.length - 1];
  const width = (hi - lo) / count || 1;
  const buckets = Array.from({ length: count }, (_, i) => ({ from: lo + i * width, to: lo + (i + 1) * width, count: 0 }));
  for (const value of sorted) {
    buckets[Math.min(count - 1, Math.floor((value - lo) / width))].count += 1;
  }
  return buckets;
}

function tally(values) {
  const counts = new Map();
  for (const value of values) {
    const key = typeof value === 'string' ? value : JSON.stringify(value);
    counts.set(key, (counts.get(key) || 0) + 1);
  }
  return [...counts.entries()].map(([label, count]) => ({ label, count })).sort((a, b) => b.count - a.count);
}

export function ScoresScreen({ go }) {
  const [range, setRange] = React.useState('7d');
  const [source, setSource] = React.useState('');
  const [name, setName] = React.useState('');
  const [cursor, setCursor] = React.useState(0);
  const window = React.useMemo(() => windowOf(range), [range]);

  const scores = useRead((signal) => api.annotations({
    since: window.sinceNs ? Math.round(window.sinceNs) : undefined,
    until: window.untilNs ? Math.round(window.untilNs) : undefined,
    source: source || undefined,
    name: name || undefined,
    limit: 500,
  }, signal), [window.sinceNs, window.untilNs, source, name]);

  const annotations = scores.data?.annotations || [];
  const summary = React.useMemo(() => summarize(annotations), [annotations]);
  const names = React.useMemo(() => [...new Set(annotations.map((a) => a.name))], [annotations]);

  // A review queue is a keyboard thing: j/k to move, Enter to open the trace
  // it belongs to. Reading a hundred scores with a mouse is nobody's evening.
  useKeys((event, { typing }) => {
    if (typing || event.metaKey || event.ctrlKey) return;
    if (event.key === 'j') { event.preventDefault(); setCursor((at) => Math.min(annotations.length - 1, at + 1)); }
    else if (event.key === 'k') { event.preventDefault(); setCursor((at) => Math.max(0, at - 1)); }
    else if (event.key === 'Enter' && annotations[cursor]) {
      event.preventDefault();
      const entry = annotations[cursor];
      go(['trace', entry.trace_id], entry.span_id ? { span: entry.span_id } : undefined);
    }
  }, [annotations, cursor, go]);

  return <div style={{ display: 'grid', gap: 14, maxWidth: 1560 }}>
    <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
      {RANGES.map((r) => <Chip key={r.id} mono active={range === r.id} onClick={() => setRange(r.id)}>{r.label}</Chip>)}
      <span style={{ marginLeft: 12, fontSize: 11, textTransform: 'uppercase', letterSpacing: 'var(--tracking-caps)', color: 'var(--ink-faint)', fontWeight: 500 }}>source</span>
      <Chip active={!source} onClick={() => setSource('')}>all</Chip>
      <Chip active={source === 'human:'} onClick={() => setSource('human:')}>human</Chip>
      <Chip active={source === 'eval:'} onClick={() => setSource('eval:')}>eval</Chip>
      {names.length ? <>
        <span style={{ marginLeft: 12, fontSize: 11, textTransform: 'uppercase', letterSpacing: 'var(--tracking-caps)', color: 'var(--ink-faint)', fontWeight: 500 }}>name</span>
        <Chip active={!name} onClick={() => setName('')}>all</Chip>
        {names.slice(0, 6).map((n) => <Chip key={n} active={name === n} onClick={() => setName(n)}>{n}</Chip>)}
      </> : null}
    </div>

    <LoadingBar active={scores.loading} />
    {scores.error ? <ErrorState what={scores.error.what} next={scores.error.next} onRetry={scores.reload} /> : null}
    {scores.data && !annotations.length ? <EmptyState
      message="No annotations in this window. Record one from any trace, or post them from an eval run:"
      command={`curl -X POST ${window.location?.origin || 'http://localhost:8080'}/v1/annotations \\
  -H 'Content-Type: application/json' \\
  -d '{"trace_id":"…","name":"groundedness","value":0.9,"source":"eval:nightly"}'`} /> : null}

    {summary.length ? <div style={{
      display: 'grid', gridTemplateColumns: `repeat(${Math.min(3, summary.length)},1fr)`, gap: 12,
    }}>
      {summary.slice(0, 3).map((entry) => <Card key={entry.name}>
        <div style={{ display: 'flex', alignItems: 'baseline', marginBottom: 10 }}>
          <Eyebrow>{entry.name}</Eyebrow>
          <span style={{ marginLeft: 'auto', fontSize: 12, color: 'var(--ink-muted)' }}>
            {fmtNum(entry.count)} scores
          </span>
        </div>
        {entry.numeric ? <>
          <div style={{ display: 'flex', alignItems: 'baseline', gap: 12, marginBottom: 10 }}>
            <span style={{
              fontFamily: 'var(--font-mono)', fontSize: 24, lineHeight: '30px', fontWeight: 500,
              fontVariantNumeric: 'tabular-nums', color: 'var(--accent)',
            }}>{entry.mean.toFixed(3)}</span>
            <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>
              mean · p50 {entry.p50?.toFixed(2)} · {entry.min?.toFixed(2)}–{entry.max?.toFixed(2)}
            </span>
          </div>
          <div style={{ display: 'flex', alignItems: 'flex-end', gap: 2, height: 54 }}>
            {entry.histogram.map((bucket, i) => {
              const max = Math.max(...entry.histogram.map((b) => b.count), 1);
              return <div key={i} title={`${bucket.from.toFixed(2)}–${bucket.to.toFixed(2)} · ${bucket.count}`}
                style={{
                  flex: 1, height: Math.max(2, (bucket.count / max) * 100) + '%',
                  background: `var(--measure-${1 + Math.min(4, Math.floor((i / entry.histogram.length) * 5))})`,
                  borderRadius: '1.5px 1.5px 0 0',
                }} />;
            })}
          </div>
        </> : <div style={{ display: 'grid', gap: 4 }}>
          {entry.tally.slice(0, 5).map((row) => <div key={row.label} style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
            <span style={{ fontFamily: 'var(--font-mono)', fontSize: 12, color: 'var(--ink)', width: 90, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
              {row.label}
            </span>
            <div style={{ flex: 1, height: 7, background: 'var(--bg-sunken)', borderRadius: 1.5, overflow: 'hidden' }}>
              <div style={{ height: '100%', width: fmtPercent(row.count, entry.count), background: 'var(--accent)', borderRadius: 1.5 }} />
            </div>
            <span style={{ fontFamily: 'var(--font-mono)', fontSize: 11, color: 'var(--ink-muted)', width: 34, textAlign: 'right' }}>
              {row.count}
            </span>
          </div>)}
        </div>}
        <div style={{ marginTop: 10, paddingTop: 8, borderTop: '1px solid var(--hairline)', fontSize: 11, color: 'var(--ink-faint)' }}>
          {entry.human} human · {entry.evaluated} eval{entry.other ? ` · ${entry.other} unattributed` : ''}
        </div>
      </Card>)}
    </div> : null}

    {annotations.length ? <Card pad="0" style={{ overflow: 'hidden' }}>
      <div style={{ padding: '10px 14px', display: 'flex', alignItems: 'baseline' }}>
        <Eyebrow>Review queue</Eyebrow>
        <span style={{ marginLeft: 'auto', fontSize: 12, color: 'var(--ink-faint)' }}>
          <Kbd>j</Kbd> <Kbd>k</Kbd> move · <Kbd>↵</Kbd> open the trace
        </span>
      </div>
      <div style={{
        display: 'grid', gridTemplateColumns: '128px 96px 1fr 132px 116px 78px',
        background: 'var(--bg-sunken)', borderTop: '1px solid var(--hairline)',
        borderBottom: '1px solid var(--hairline)',
      }}>
        {['name', 'value', 'comment', 'source', 'trace', 'when'].map((label, i) => <div key={label} style={{
          padding: '6px 10px', fontSize: 12, fontWeight: 500, color: 'var(--ink-muted)',
          textAlign: i === 5 ? 'right' : 'left', whiteSpace: 'nowrap',
        }}>{label}</div>)}
      </div>
      {annotations.slice(0, 200).map((entry, index) => <div key={index}
        onClick={() => go(['trace', entry.trace_id], entry.span_id ? { span: entry.span_id } : undefined)}
        role="link" tabIndex={0}
        onKeyDown={(e) => { if (e.key === 'Enter') go(['trace', entry.trace_id]); }}
        style={{
          display: 'grid', gridTemplateColumns: '128px 96px 1fr 132px 116px 78px',
          alignItems: 'center', borderBottom: '1px solid var(--hairline)', cursor: 'pointer',
          minHeight: 'var(--row-h)',
          background: index === cursor ? 'var(--bg-sunken)' : 'transparent',
          borderLeft: `2px solid ${index === cursor ? 'var(--accent)' : 'transparent'}`,
        }}>
        <Cell mono>{entry.name}</Cell>
        <Cell mono accent>{typeof entry.value === 'number' ? entry.value : JSON.stringify(entry.value)}</Cell>
        <Cell muted>{entry.comment || '—'}</Cell>
        <Cell mono muted>{entry.source || '—'}</Cell>
        <Cell mono muted>{entry.trace_id.slice(0, 14)}</Cell>
        <Cell muted align="right">{fmtAgo(entry.timestamp_ns)}</Cell>
      </div>)}
    </Card> : null}
  </div>;
}

function Cell({ children, mono, muted, accent, align }) {
  return <div style={{
    padding: 'var(--row-py) 10px',
    fontFamily: mono ? 'var(--font-mono)' : 'inherit',
    fontSize: mono ? 12 : 'var(--cell-fs)',
    fontVariantNumeric: mono ? 'tabular-nums' : undefined,
    color: accent ? 'var(--accent)' : muted ? 'var(--ink-muted)' : 'var(--ink)',
    textAlign: align || 'left',
    overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
  }}>{children}</div>;
}
