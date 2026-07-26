import React from 'react';
import { api } from '../lib/api.js';
import { useRead } from '../lib/route.js';
import { RANGES, windowOf } from '../lib/query.js';
import { fmtAgo, fmtClockNs, fmtDurationNs, fmtNum, fmtPercent } from '../lib/format.js';
import { Card, Chip, Eyebrow, ErrorState, EmptyState, LoadingBar, Mono } from '../components/primitives/Chrome.jsx';
import { StackedSparkbar, TimeAxis } from '../components/charts/Marks.jsx';

// Errors grouped by signature. "3 errors" used to be a red number that did
// nothing; every count here opens the spans behind it, because the distance
// between noticing a failure and reading one should be a click.
//
// The grouping happens on the server: the input can be every error in the
// window and the useful answer is a dozen rows, so shipping the spans here to
// group them in the browser would move megabytes to fill one screen.

export function FailuresScreen({ go }) {
  const [range, setRange] = React.useState('24h');
  const window = React.useMemo(() => windowOf(range), [range]);
  const base = React.useMemo(() => ({
    since: window.sinceNs ? Math.round(window.sinceNs) : undefined,
    until: window.untilNs ? Math.round(window.untilNs) : undefined,
  }), [window.sinceNs, window.untilNs]);

  const failures = useRead((signal) => api.failures({ ...base, limit: 50 }, signal), [JSON.stringify(base)]);
  const series = useRead(
    (signal) => api.series({ since: Math.round(window.sinceNs), until: Math.round(window.untilNs), buckets: 48 }, signal),
    [JSON.stringify(base)],
    { skip: !window.sinceNs },
  );

  const groups = failures.data?.groups || [];
  const total = groups.reduce((sum, group) => sum + group.count, 0);
  const buckets = series.data?.buckets || [];
  const spans = buckets.reduce((sum, b) => sum + b.spans, 0);
  const errors = buckets.reduce((sum, b) => sum + b.errors, 0);

  return <div style={{ display: 'grid', gap: 14, maxWidth: 1560 }}>
    <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
      {RANGES.map((r) => <Chip key={r.id} mono active={range === r.id} onClick={() => setRange(r.id)}>{r.label}</Chip>)}
      <span style={{ marginLeft: 'auto', fontSize: 12, color: 'var(--ink-muted)' }}>
        {spans ? <>
          <Mono color="var(--error)">{fmtNum(errors)}</Mono> of <Mono>{fmtNum(spans)}</Mono> spans failed
          {' '}(<Mono color="var(--error)">{fmtPercent(errors, spans)}</Mono>)
        </> : null}
      </span>
    </div>

    <LoadingBar active={failures.loading} />
    {failures.error ? <ErrorState what={failures.error.what} next={failures.error.next} onRetry={failures.reload} /> : null}

    {buckets.length ? <Card pad="10px 14px 12px">
      <div style={{ display: 'flex', alignItems: 'baseline', marginBottom: 8 }}>
        <Eyebrow>Failures over time</Eyebrow>
        <span style={{ marginLeft: 10, fontSize: 12, color: 'var(--ink-faint)' }}>
          errors above the line, everything else below
        </span>
      </div>
      <StackedSparkbar buckets={buckets} height={64} />
      {series.data ? <TimeAxis sinceNs={series.data.since_ns} untilNs={series.data.until_ns} /> : null}
    </Card> : null}

    {failures.data && !groups.length ? <EmptyState
      message="Nothing failed in this window. A span counts as a failure when its status is exactly `error`." /> : null}

    {groups.length ? <Card pad="0" style={{ overflow: 'hidden' }}>
      <div style={{
        display: 'grid', gridTemplateColumns: '72px 1fr 132px 96px 88px 88px 104px',
        background: 'var(--bg-sunken)', borderBottom: '1px solid var(--hairline)',
      }}>
        {['count', 'signature', 'share', 'p50', 'p95', 'first seen', 'last seen'].map((label, i) => (
          <div key={label} style={{
            padding: '6px 10px', fontSize: 12, fontWeight: 500, color: 'var(--ink-muted)',
            textAlign: i === 0 || i > 2 ? 'right' : 'left', whiteSpace: 'nowrap',
          }}>{label}</div>
        ))}
      </div>
      {groups.map((group) => (
        <div key={`${group.service}/${group.name}/${group.status}`}
          onClick={() => go(['trace', group.example_trace_id], { span: group.example_span_id })}
          role="link" tabIndex={0}
          onKeyDown={(e) => { if (e.key === 'Enter') go(['trace', group.example_trace_id], { span: group.example_span_id }); }}
          style={{
            display: 'grid', gridTemplateColumns: '72px 1fr 132px 96px 88px 88px 104px',
            alignItems: 'center', borderBottom: '1px solid var(--hairline)',
            cursor: 'pointer', minHeight: 'var(--row-h)',
          }}>
          <div style={{
            padding: 'var(--row-py) 10px', fontFamily: 'var(--font-mono)', fontSize: 12,
            fontVariantNumeric: 'tabular-nums', color: 'var(--error)', textAlign: 'right',
          }}>{fmtNum(group.count)}</div>
          <div style={{ padding: 'var(--row-py) 10px', minWidth: 0 }}>
            <div style={{
              fontSize: 'var(--cell-fs)', color: 'var(--ink)',
              overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
            }}>
              {group.service} <span style={{ color: 'var(--ink-faint)' }}>·</span> {group.name}
              <span style={{
                marginLeft: 8, fontFamily: 'var(--font-mono)', fontSize: 11, padding: '0 5px',
                borderRadius: 'var(--radius-control)', background: 'var(--error-tint)', color: 'var(--error)',
              }}>{group.status}</span>
            </div>
          </div>
          <div style={{ padding: 'var(--row-py) 10px', display: 'flex', alignItems: 'center', gap: 7 }}>
            <div style={{ flex: 1, height: 7, background: 'var(--bg-sunken)', borderRadius: 1.5, overflow: 'hidden' }}>
              <div style={{
                height: '100%', width: total ? (group.count / total) * 100 + '%' : 0,
                background: 'var(--error)', borderRadius: 1.5,
              }} />
            </div>
            <span style={{
              fontFamily: 'var(--font-mono)', fontSize: 11, fontVariantNumeric: 'tabular-nums',
              color: 'var(--ink-muted)', width: 32, textAlign: 'right',
            }}>{fmtPercent(group.count, total)}</span>
          </div>
          <Num>{fmtDurationNs(group.p50_ns)}</Num>
          <Num>{fmtDurationNs(group.p95_ns)}</Num>
          <Num muted>{fmtClockNs(group.first_seen_ns)}</Num>
          <Num muted>{fmtAgo(group.last_seen_ns)} ago</Num>
        </div>
      ))}
    </Card> : null}
  </div>;
}

function Num({ children, muted }) {
  return <div style={{
    padding: 'var(--row-py) 10px', fontFamily: 'var(--font-mono)', fontSize: 12,
    fontVariantNumeric: 'tabular-nums', color: muted ? 'var(--ink-muted)' : 'var(--ink)',
    textAlign: 'right', whiteSpace: 'nowrap',
  }}>{children}</div>;
}
