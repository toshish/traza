import React from 'react';
import { api } from '../lib/api.js';
import { useRead } from '../lib/route.js';
import { RANGES, windowOf } from '../lib/query.js';
import { fmtClockNs, fmtDurationNs, fmtNum } from '../lib/format.js';
import { Card, Chip, Eyebrow, ErrorState, EmptyState, LoadingBar, Skeleton } from '../components/primitives/Chrome.jsx';
import { Distribution, Sparkbar, TimeAxis } from '../components/charts/Marks.jsx';

// The distribution, the percentiles, and the traces behind the tail. A mean
// latency hides every tail, and the tail is the only part anybody is paid to
// care about — so this screen never shows one.

export function LatencyScreen({ go, params }) {
  const [range, setRange] = React.useState('24h');
  const [service, setService] = React.useState('');
  const window = React.useMemo(() => windowOf(range), [range]);
  const base = React.useMemo(() => ({
    since: window.sinceNs ? Math.round(window.sinceNs) : undefined,
    until: window.untilNs ? Math.round(window.untilNs) : undefined,
    service: service || undefined,
  }), [window.sinceNs, window.untilNs, service]);

  const dist = useRead((signal) => api.duration(base, signal), [JSON.stringify(base)]);
  const slowest = useRead((signal) => api.slowest({ ...base, limit: 10 }, signal), [JSON.stringify(base)]);
  const series = useRead(
    (signal) => api.series({ ...base, since: Math.round(window.sinceNs), until: Math.round(window.untilNs), buckets: 48 }, signal),
    [JSON.stringify(base)],
    { skip: !window.sinceNs },
  );
  const services = useRead((signal) => api.llmStats({ group_by: 'service', limit: 12 }, signal), []);

  const data = dist.data;
  const buckets = series.data?.buckets || [];

  return <div style={{ display: 'grid', gap: 14, maxWidth: 1560 }}>
    <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
      {RANGES.map((r) => <Chip key={r.id} mono active={range === r.id} onClick={() => setRange(r.id)}>{r.label}</Chip>)}
      <span style={{ marginLeft: 12, fontSize: 11, textTransform: 'uppercase', letterSpacing: 'var(--tracking-caps)', color: 'var(--ink-faint)', fontWeight: 500 }}>service</span>
      <Chip active={!service} onClick={() => setService('')}>all</Chip>
      {(services.data?.rows || []).slice(0, 6).map((row) => (
        <Chip key={row.key} active={service === row.key} onClick={() => setService(row.key)}>{row.key}</Chip>
      ))}
    </div>

    <LoadingBar active={dist.loading} />
    {dist.error ? <ErrorState what={dist.error.what} next={dist.error.next} onRetry={dist.reload} /> : null}

    {data && data.count === 0 ? <EmptyState
      message="No spans in this window, so there is no distribution to draw." /> : null}

    {data && data.count > 0 ? <>
      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(6,1fr)', gap: 12 }}>
        {[
          ['spans', fmtNum(data.count), 'var(--ink)'],
          ['p50', fmtDurationNs(data.p50_ns)],
          ['p90', fmtDurationNs(data.p90_ns)],
          ['p95', fmtDurationNs(data.p95_ns)],
          ['p99', fmtDurationNs(data.p99_ns)],
          ['max', fmtDurationNs(data.max_ns)],
        ].map(([label, value, tone]) => <Card key={label} pad="12px 14px">
          <div style={{ fontSize: 12, color: 'var(--ink-muted)', marginBottom: 5 }}>{label}</div>
          <div style={{
            fontFamily: 'var(--font-mono)', fontSize: 20, lineHeight: '28px', fontWeight: 500,
            fontVariantNumeric: 'tabular-nums', color: tone || 'var(--accent)',
          }}>{value}</div>
        </Card>)}
      </div>

      <Card>
        <div style={{ display: 'flex', alignItems: 'baseline', marginBottom: 4 }}>
          <Eyebrow>Distribution</Eyebrow>
          <span style={{ marginLeft: 10, fontSize: 12, color: 'var(--ink-faint)' }}>
            log-spaced buckets · percentiles are bucket upper bounds, within 6.25% and never low
          </span>
        </div>
        <Distribution buckets={data.buckets} p50Ns={data.p50_ns} p95Ns={data.p95_ns} p99Ns={data.p99_ns} />
      </Card>

      <Card>
        <div style={{ display: 'flex', alignItems: 'baseline', marginBottom: 10 }}>
          <Eyebrow>p95 over time</Eyebrow>
          <span style={{ marginLeft: 10, fontSize: 12, color: 'var(--ink-faint)' }}>
            each bar is one bucket's 95th percentile, not its mean
          </span>
        </div>
        <Sparkbar values={buckets.map((b) => b.p95_ns)} height={90} color="var(--measure-3)" />
        {series.data ? <TimeAxis sinceNs={series.data.since_ns} untilNs={series.data.until_ns} /> : null}
      </Card>

      <Card pad="12px 14px">
        <div style={{ display: 'flex', alignItems: 'baseline', marginBottom: 10 }}>
          <Eyebrow>The tail</Eyebrow>
          <span style={{ marginLeft: 10, fontSize: 12, color: 'var(--ink-faint)' }}>
            the ten slowest spans in this window — ranked by the server, not by the page
          </span>
        </div>
        {slowest.loading && !slowest.data ? <Skeleton height={120} /> : null}
        {(slowest.data?.spans || []).map((span) => {
          const duration = span.end_time_ns - span.start_time_ns;
          return <div key={span.trace_id + span.span_id}
            onClick={() => go(['trace', span.trace_id], { span: span.span_id })}
            role="link" tabIndex={0}
            onKeyDown={(e) => { if (e.key === 'Enter') go(['trace', span.trace_id], { span: span.span_id }); }}
            style={{
              display: 'grid', gridTemplateColumns: '92px 140px 1fr 96px 82px', gap: 10,
              alignItems: 'center', padding: 'var(--row-py) 4px', minHeight: 'var(--row-h)',
              borderBottom: '1px solid var(--hairline)', cursor: 'pointer',
            }}>
            <span style={{ fontFamily: 'var(--font-mono)', fontSize: 12, color: 'var(--ink-muted)' }}>
              {fmtClockNs(span.start_time_ns)}
            </span>
            <span style={{ fontSize: 'var(--cell-fs)', color: 'var(--ink)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
              {span.service}
            </span>
            <span style={{ fontSize: 'var(--cell-fs)', color: 'var(--ink)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
              {span.name}
            </span>
            <span style={{
              fontFamily: 'var(--font-mono)', fontSize: 12, fontVariantNumeric: 'tabular-nums',
              color: 'var(--accent)', textAlign: 'right',
            }}>{fmtDurationNs(duration)}</span>
            <span style={{
              fontFamily: 'var(--font-mono)', fontSize: 11, color: 'var(--ink-faint)',
              textAlign: 'right', overflow: 'hidden', textOverflow: 'ellipsis',
            }}>{span.trace_id.slice(0, 10)}</span>
          </div>;
        })}
      </Card>
    </> : null}
  </div>;
}
