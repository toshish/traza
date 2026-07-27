import React from 'react';
import { api } from '../lib/api.js';
import { useRead } from '../lib/route.js';
import { RANGES, windowOf } from '../lib/query.js';
import { fmtDurationNs, fmtNum } from '../lib/format.js';
import { Card, Chip, Eyebrow, ErrorState, EmptyState, LoadingBar } from '../components/primitives/Chrome.jsx';
import { Distribution } from '../components/charts/Marks.jsx';

// A/B two attribute cohorts on latency, cost and error rate. Everything here
// is two filtered reads of endpoints that already exist — a cohort is just a
// predicate, which is what makes this cheap to offer at all.

function Delta({ a, b, format, lowerIsBetter = true }) {
  if (!a || !b) return <span style={{ color: 'var(--ink-faint)' }}>—</span>;
  const change = (b - a) / a;
  if (!Number.isFinite(change)) return <span style={{ color: 'var(--ink-faint)' }}>—</span>;
  const better = lowerIsBetter ? change < 0 : change > 0;
  const flat = Math.abs(change) < 0.02;
  return <span style={{
    fontFamily: 'var(--font-mono)', fontSize: 12, fontVariantNumeric: 'tabular-nums',
    color: flat ? 'var(--ink-muted)' : better ? 'var(--ok)' : 'var(--error)',
  }}>
    {change >= 0 ? '+' : '−'}{Math.abs(change * 100).toFixed(1)}%
  </span>;
}

function useCohort(field, value, window, enabled) {
  const base = React.useMemo(() => {
    if (!enabled || !value) return null;
    const params = {
      since: window.sinceNs ? Math.round(window.sinceNs) : undefined,
      until: window.untilNs ? Math.round(window.untilNs) : undefined,
    };
    if (field === 'service') params.service = value;
    else if (field === 'name') params.name = value;
    else params['attr.' + field] = value;
    return params;
  }, [field, value, window.sinceNs, window.untilNs, enabled]);

  const duration = useRead((signal) => api.duration(base, signal), [JSON.stringify(base)], { skip: !base });
  const failures = useRead((signal) => api.failures({ ...base, limit: 200 }, signal), [JSON.stringify(base)], { skip: !base });
  const errors = (failures.data?.groups || []).reduce((total, group) => total + group.count, 0);
  return { duration: duration.data, errors, loading: duration.loading, error: duration.error };
}

export function ExperimentsScreen() {
  const [range, setRange] = React.useState('7d');
  const [field, setField] = React.useState('gen_ai.request.model');
  const [a, setA] = React.useState('');
  const [b, setB] = React.useState('');
  const window = React.useMemo(() => windowOf(range), [range]);

  const models = useRead((signal) => api.llmStats({ group_by: 'model', limit: 12 }, signal), []);
  const candidates = (models.data?.rows || []).map((row) => row.key);

  // Two obvious cohorts beat an empty form: the two costliest models are
  // almost always the comparison somebody came here to make.
  React.useEffect(() => {
    if (!a && candidates[0]) setA(candidates[0]);
    if (!b && candidates[1]) setB(candidates[1]);
  }, [candidates.join('|')]);

  const cohortA = useCohort(field, a, window, true);
  const cohortB = useCohort(field, b, window, true);

  const rows = [
    ['spans', (c) => c.duration?.count, fmtNum, false],
    ['p50', (c) => c.duration?.p50_ns, fmtDurationNs, true],
    ['p95', (c) => c.duration?.p95_ns, fmtDurationNs, true],
    ['p99', (c) => c.duration?.p99_ns, fmtDurationNs, true],
    ['max', (c) => c.duration?.max_ns, fmtDurationNs, true],
    ['errors', (c) => c.errors, fmtNum, true],
  ];

  const picker = (value, onChange, label) => <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
    <span style={{ fontSize: 11, textTransform: 'uppercase', letterSpacing: 'var(--tracking-caps)', color: 'var(--ink-faint)', fontWeight: 500 }}>
      {label}
    </span>
    <input value={value} onChange={(event) => onChange(event.target.value)} list="traza-cohorts"
      aria-label={`Cohort ${label}`}
      style={{
        padding: '3px 8px', border: '1px solid var(--hairline)', borderRadius: 'var(--radius-control)',
        background: 'var(--bg)', fontFamily: 'var(--font-mono)', fontSize: 12, color: 'var(--ink)',
        outline: 'none', width: 200,
      }} />
  </div>;

  return <div style={{ display: 'grid', gap: 14, maxWidth: 1400 }}>
    <datalist id="traza-cohorts">{candidates.map((c) => <option key={c} value={c} />)}</datalist>

    <div style={{ display: 'flex', alignItems: 'center', gap: 10, flexWrap: 'wrap' }}>
      <span style={{ fontSize: 11, textTransform: 'uppercase', letterSpacing: 'var(--tracking-caps)', color: 'var(--ink-faint)', fontWeight: 500 }}>
        split by
      </span>
      {['gen_ai.request.model', 'service', 'name'].map((f) => (
        <Chip key={f} mono active={field === f} onClick={() => { setField(f); setA(''); setB(''); }}>{f}</Chip>
      ))}
      <span style={{ marginLeft: 'auto', display: 'flex', gap: 6 }}>
        {RANGES.map((r) => <Chip key={r.id} mono active={range === r.id} onClick={() => setRange(r.id)}>{r.label}</Chip>)}
      </span>
    </div>

    <div style={{ display: 'flex', alignItems: 'center', gap: 20, flexWrap: 'wrap' }}>
      {picker(a, setA, 'A')}
      {picker(b, setB, 'B')}
    </div>

    <LoadingBar active={cohortA.loading || cohortB.loading} />
    {cohortA.error ? <ErrorState what={cohortA.error.what} next={cohortA.error.next} /> : null}

    {!a || !b ? <EmptyState message="Pick two cohorts to compare. Each one is a predicate, so anything you can filter by, you can A/B." /> : null}

    {a && b && cohortA.duration && cohortB.duration ? <>
      <Card pad="0" style={{ overflow: 'hidden' }}>
        <div style={{
          display: 'grid', gridTemplateColumns: '140px 1fr 1fr 110px',
          background: 'var(--bg-sunken)', borderBottom: '1px solid var(--hairline)',
        }}>
          <div style={{ padding: '8px 12px', fontSize: 12, fontWeight: 500, color: 'var(--ink-muted)' }}>measure</div>
          <div style={{ padding: '8px 12px', fontSize: 12, fontWeight: 500, color: 'var(--ink)', fontFamily: 'var(--font-mono)' }}>A · {a}</div>
          <div style={{ padding: '8px 12px', fontSize: 12, fontWeight: 500, color: 'var(--ink)', fontFamily: 'var(--font-mono)' }}>B · {b}</div>
          <div style={{ padding: '8px 12px', fontSize: 12, fontWeight: 500, color: 'var(--ink-muted)', textAlign: 'right' }}>B vs A</div>
        </div>
        {rows.map(([label, read, format, lowerIsBetter]) => {
          const left = read(cohortA);
          const right = read(cohortB);
          const max = Math.max(left || 0, right || 0, 1);
          return <div key={label} style={{
            display: 'grid', gridTemplateColumns: '140px 1fr 1fr 110px', alignItems: 'center',
            borderBottom: '1px solid var(--hairline)', minHeight: 'var(--row-h)',
          }}>
            <div style={{ padding: 'var(--row-py) 12px', fontSize: 'var(--cell-fs)', color: 'var(--ink-muted)' }}>{label}</div>
            {[left, right].map((value, index) => <div key={index} style={{
              padding: 'var(--row-py) 12px', display: 'flex', alignItems: 'center', gap: 9,
            }}>
              <div style={{ flex: 1, height: 7, background: 'var(--bg-sunken)', borderRadius: 1.5, overflow: 'hidden' }}>
                <div style={{
                  height: '100%', width: Math.max(1, ((value || 0) / max) * 100) + '%',
                  background: index === 0 ? 'var(--accent)' : 'var(--series-2)', borderRadius: 1.5,
                }} />
              </div>
              <span style={{
                fontFamily: 'var(--font-mono)', fontSize: 12, fontVariantNumeric: 'tabular-nums',
                color: 'var(--ink)', width: 84, textAlign: 'right',
              }}>{value == null ? '—' : format(value)}</span>
            </div>)}
            <div style={{ padding: 'var(--row-py) 12px', textAlign: 'right' }}>
              <Delta a={left} b={right} lowerIsBetter={lowerIsBetter} />
            </div>
          </div>;
        })}
      </Card>

      <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 12 }}>
        {[[a, cohortA], [b, cohortB]].map(([label, cohort]) => (
          <Card key={label}>
            <div style={{ display: 'flex', alignItems: 'baseline', marginBottom: 4 }}>
              <Eyebrow>{label}</Eyebrow>
              <span style={{ marginLeft: 'auto', fontSize: 12, color: 'var(--ink-muted)' }}>
                {fmtNum(cohort.duration?.count || 0)} spans
              </span>
            </div>
            {cohort.duration?.buckets?.length
              ? <Distribution buckets={cohort.duration.buckets} p50Ns={cohort.duration.p50_ns}
                  p95Ns={cohort.duration.p95_ns} height={120} />
              : <div style={{ fontSize: 13, color: 'var(--ink-muted)', padding: '12px 0' }}>No spans in this cohort.</div>}
          </Card>
        ))}
      </div>
    </> : null}
  </div>;
}
