import React from 'react';
import { api } from '../lib/api.js';
import { useRead, usePoll } from '../lib/route.js';
import {
  durabilityMeans, fmtBytes, fmtDurationNs, fmtNum, fmtPercent, fmtRate, fmtUptime,
} from '../lib/format.js';
import { Card, Eyebrow, ErrorState, Mono, Skeleton } from '../components/primitives/Chrome.jsx';
import { Sparkbar, ShareBar } from '../components/charts/Marks.jsx';

// The product's best quality used to be invisible inside the product: the
// README claimed p95 3.3 ms and the dashboard never showed how long anything
// took. Every figure here is counted by this process — not a benchmark
// constant, and not a number the page made up.
//
// Percentiles are bucket upper bounds with a stated error bound, which the
// screen says out loud rather than implying an exactness the buckets do not
// have.

const SAMPLES = 40;

function Stat({ label, value, unit, tone, note }) {
  return <div style={{ borderLeft: '2px solid var(--hairline)', paddingLeft: 10, minWidth: 0 }}>
    <div style={{ fontSize: 11, color: 'var(--ink-muted)', marginBottom: 2 }}>{label}</div>
    <div style={{ display: 'flex', alignItems: 'baseline', gap: 4 }}>
      <span style={{
        fontFamily: 'var(--font-mono)', fontSize: 16, fontVariantNumeric: 'tabular-nums',
        color: tone || 'var(--ink)', whiteSpace: 'nowrap',
      }}>{value}</span>
      {unit ? <span style={{ fontFamily: 'var(--font-mono)', fontSize: 11, color: 'var(--ink-muted)' }}>{unit}</span> : null}
    </div>
    {note ? <div style={{ fontSize: 11, color: 'var(--ink-faint)', marginTop: 1 }}>{note}</div> : null}
  </div>;
}

export function ServerScreen() {
  const metrics = useRead((signal) => api.metrics(signal), []);
  const stats = useRead((signal) => api.stats(signal), []);
  const [history, setHistory] = React.useState([]);
  const previous = React.useRef(null);

  // The rate is differenced client-side from the admitted counter: the server
  // keeps a counter, which is the honest thing to keep, and a rate is a view
  // of it rather than a second piece of state to get wrong.
  usePoll(async () => {
    try {
      const now = await api.metrics();
      const at = performance.now();
      const last = previous.current;
      previous.current = { admitted: now.ingest.spans_admitted, requests: now.requests.total, at };
      if (last) {
        const seconds = Math.max(0.001, (at - last.at) / 1000);
        setHistory((all) => [...all, {
          ingest: Math.max(0, (now.ingest.spans_admitted - last.admitted) / seconds),
          queries: Math.max(0, (now.requests.total - last.requests) / seconds),
        }].slice(-SAMPLES));
      }
      metrics.reload();
    } catch (e) { /* the screen keeps its last good reading */ }
  }, 2000);

  const m = metrics.data;
  if (metrics.error) return <ErrorState what={metrics.error.what} next={metrics.error.next} onRetry={metrics.reload} />;
  if (!m) return <Skeleton height={220} />;

  const errorBound = fmtPercent(m.percentile_error_bound, 1);
  const classes = [
    ['search', 'filtered span search'],
    ['lookup', 'trace, session, payload by id'],
    ['stats', 'aggregation'],
    ['ingest', 'span ingest'],
    ['other', 'dashboard and metrics'],
  ];

  return <div style={{ display: 'grid', gap: 14, maxWidth: 1560 }}>
    <Card>
      <div style={{ fontSize: 13, lineHeight: '21px', color: 'var(--ink)', textWrap: 'pretty' }}>
        This server has been up <Mono color="var(--accent)">{fmtUptime(m.uptime_ns)}</Mono>, admitted{' '}
        <Mono color="var(--accent)">{fmtNum(m.ingest.spans_admitted)}</Mono> spans, and answered{' '}
        <Mono color="var(--accent)">{fmtNum(m.requests.total)}</Mono> requests at{' '}
        <Mono color="var(--accent)">p95 {fmtDurationNs(m.requests.p95_ns)}</Mono>. Durability is{' '}
        <Mono>{m.durability || stats.data?.durability || '—'}</Mono> —{' '}
        {durabilityMeans(m.durability || stats.data?.durability)}. Every figure here is counted by
        this process, not estimated.
      </div>
      <div style={{
        marginTop: 10, paddingTop: 10, borderTop: '1px solid var(--hairline)',
        fontSize: 12, color: 'var(--ink-muted)',
      }}>
        Percentiles are the upper bound of a log-linear bucket: at most{' '}
        <Mono>{errorBound}</Mono> high, never low. Means and maxima are exact.
      </div>
    </Card>

    <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 12 }}>
      <Card>
        <div style={{ display: 'flex', alignItems: 'baseline', marginBottom: 10 }}>
          <Eyebrow>ingest</Eyebrow>
          <span style={{
            marginLeft: 'auto', fontFamily: 'var(--font-mono)', fontSize: 12,
            fontVariantNumeric: 'tabular-nums', color: 'var(--accent)',
          }}>{history.length ? fmtRate(history[history.length - 1].ingest) : '—'}</span>
        </div>
        <Sparkbar values={history.map((h) => h.ingest)} height={54} />
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(3,1fr)', gap: 10, marginTop: 12 }}>
          <Stat label="spans admitted" value={fmtNum(m.ingest.spans_admitted)} />
          <Stat label="batches" value={fmtNum(m.ingest.batches_admitted)}
            note={m.ingest.batches_admitted ? `${Math.round(m.ingest.spans_admitted / m.ingest.batches_admitted)} spans/batch` : ''} />
          <Stat label="decode p95" value={fmtDurationNs(m.decode.p95_ns)} />
        </div>
      </Card>

      <Card>
        <div style={{ display: 'flex', alignItems: 'baseline', marginBottom: 10 }}>
          <Eyebrow>queries</Eyebrow>
          <span style={{
            marginLeft: 'auto', fontFamily: 'var(--font-mono)', fontSize: 12,
            fontVariantNumeric: 'tabular-nums', color: 'var(--accent)',
          }}>{history.length ? fmtRate(history[history.length - 1].queries) : '—'}</span>
        </div>
        <Sparkbar values={history.map((h) => h.queries)} height={54} />
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(3,1fr)', gap: 10, marginTop: 12 }}>
          <Stat label="answered" value={fmtNum(m.requests.total)} />
          <Stat label="4xx" value={fmtNum(m.requests.responses_4xx)}
            tone={m.requests.responses_4xx ? 'var(--warn)' : undefined} />
          <Stat label="5xx" value={fmtNum(m.requests.responses_5xx)}
            tone={m.requests.responses_5xx ? 'var(--error)' : undefined} />
        </div>
      </Card>
    </div>

    <Card pad="0" style={{ overflow: 'hidden' }}>
      <div style={{ padding: '12px 14px 10px' }}>
        <Eyebrow>Latency by route class</Eyebrow>
        <span style={{ marginLeft: 10, fontSize: 12, color: 'var(--ink-faint)' }}>
          one histogram per class — an ingest batch and a trace lookup differ by orders of
          magnitude, and one blended figure described neither
        </span>
      </div>
      <div style={{
        display: 'grid', gridTemplateColumns: '160px 1fr 88px 88px 88px 88px 92px',
        background: 'var(--bg-sunken)', borderTop: '1px solid var(--hairline)',
        borderBottom: '1px solid var(--hairline)',
      }}>
        {['class', '', 'count', 'mean', 'p50', 'p95', 'p99'].map((label, i) => <div key={i} style={{
          padding: '6px 10px', fontSize: 12, fontWeight: 500, color: 'var(--ink-muted)',
          textAlign: i > 1 ? 'right' : 'left', whiteSpace: 'nowrap',
        }}>{label}</div>)}
      </div>
      {classes.map(([id, description]) => {
        const row = m.by_class[id];
        if (!row) return null;
        const worst = Math.max(...Object.values(m.by_class).map((c) => c.p95_ns), 1);
        return <div key={id} style={{
          display: 'grid', gridTemplateColumns: '160px 1fr 88px 88px 88px 88px 92px',
          alignItems: 'center', borderBottom: '1px solid var(--hairline)', minHeight: 'var(--row-h)',
        }}>
          <div style={{ padding: 'var(--row-py) 10px' }}>
            <div style={{ fontFamily: 'var(--font-mono)', fontSize: 12, color: 'var(--ink)' }}>{id}</div>
            <div style={{ fontSize: 11, color: 'var(--ink-faint)' }}>{description}</div>
          </div>
          <div style={{ padding: 'var(--row-py) 10px' }}>
            <div style={{ height: 7, background: 'var(--bg-sunken)', borderRadius: 1.5, overflow: 'hidden' }}>
              <div style={{
                height: '100%', width: Math.max(1, (row.p95_ns / worst) * 100) + '%',
                background: 'var(--accent)', borderRadius: 1.5,
              }} />
            </div>
          </div>
          <Num muted>{fmtNum(row.count)}</Num>
          <Num muted>{row.count ? fmtDurationNs(row.mean_ns) : '—'}</Num>
          <Num>{row.count ? fmtDurationNs(row.p50_ns) : '—'}</Num>
          <Num accent>{row.count ? fmtDurationNs(row.p95_ns) : '—'}</Num>
          <Num muted>{row.count ? fmtDurationNs(row.p99_ns) : '—'}</Num>
        </div>;
      })}
    </Card>

    <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 12 }}>
      <Card>
        <Eyebrow style={{ marginBottom: 10 }}>Time-range pruning</Eyebrow>
        <div style={{ fontSize: 13, lineHeight: '20px', color: 'var(--ink)', marginBottom: 10, textWrap: 'pretty' }}>
          A segment whose timestamp range cannot hold a match is skipped without being read.
          Across every query this process has served,{' '}
          <Mono color="var(--accent)">
            {fmtPercent(m.pruning.segments_pruned_by_time, m.pruning.segments_examined || 1)}
          </Mono> of segments examined were eliminated that way.
        </div>
        <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
          <ShareBar part={m.pruning.segments_examined - m.pruning.segments_pruned_by_time}
            whole={m.pruning.segments_examined || 1} width={220} />
          <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>
            {fmtNum(m.pruning.segments_examined - m.pruning.segments_pruned_by_time)} read
            of {fmtNum(m.pruning.segments_examined)} examined
          </span>
        </div>
      </Card>

      <Card>
        <Eyebrow style={{ marginBottom: 10 }}>Durability and connections</Eyebrow>
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(2,1fr)', gap: 12 }}>
          <Stat label="durability" value={m.durability || stats.data?.durability || '—'}
            tone={(m.durability || stats.data?.durability) === 'buffered' ? 'var(--warn)' : undefined}
            note={(m.durability || stats.data?.durability) === 'buffered'
              ? 'acknowledged writes are not durable' : 'what an acknowledged write guarantees'} />
          <Stat label="WAL bytes" value={stats.data ? fmtBytes(stats.data.wal_bytes) : '—'}
            note="the work a restart would replay" />
          <Stat label="wal fsync p95" value={fmtDurationNs(m.ingest.wal_fsync_p95_ns)} />
          <Stat label="segment seal p95" value={fmtDurationNs(m.ingest.segment_seal_p95_ns)} />
          <Stat label="connections live" value={fmtNum(m.connections.live)} />
          <Stat label="refused" value={fmtNum(m.connections.refused)}
            tone={m.connections.refused ? 'var(--error)' : undefined}
            note={m.connections.refused ? 'clients were shed — throughput measured here is suspect' : 'no backpressure'} />
        </div>
      </Card>
    </div>
  </div>;
}

function Num({ children, muted, accent }) {
  return <div style={{
    padding: 'var(--row-py) 10px', fontFamily: 'var(--font-mono)', fontSize: 12,
    fontVariantNumeric: 'tabular-nums', textAlign: 'right', whiteSpace: 'nowrap',
    color: accent ? 'var(--accent)' : muted ? 'var(--ink-muted)' : 'var(--ink)',
  }}>{children}</div>;
}
