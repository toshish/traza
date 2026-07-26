import React from 'react';
import { api } from '../lib/api.js';
import { useRead } from '../lib/route.js';
import { RANGES, windowOf } from '../lib/query.js';
import {
  fmtCompact, fmtCost, fmtDelta, fmtDurationNs, fmtNum, fmtPercent, fmtAgo, fmtClockNs, fmtWindowLabel,
} from '../lib/format.js';
import { Card, Chip, Eyebrow, ErrorState, LiveDot, LoadingBar, Skeleton } from '../components/primitives/Chrome.jsx';
import { Sparkbar, StackedSparkbar } from '../components/charts/Marks.jsx';

// The screen that answers "where should I look" before you have decided what
// to look at. Everything on it is a link into the screen that explains it —
// a number nobody can click is a number nobody can act on.

function Tile({ label, value, unit, delta, deltaTone, spark, highlight }) {
  return <Card pad="12px 14px 10px">
    <div style={{ display: 'flex', alignItems: 'baseline', gap: 8, marginBottom: 5 }}>
      <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>{label}</span>
      <span style={{
        marginLeft: 'auto', fontFamily: 'var(--font-mono)', fontSize: 11,
        fontVariantNumeric: 'tabular-nums',
        color: deltaTone === 'warn' ? 'var(--warn)' : deltaTone === 'error' ? 'var(--error)' : 'var(--ink-faint)',
      }}>{delta}</span>
    </div>
    <div style={{ display: 'flex', alignItems: 'baseline', gap: 5, marginBottom: 9 }}>
      <span style={{
        fontFamily: 'var(--font-mono)', fontSize: 24, lineHeight: '30px', fontWeight: 500,
        fontVariantNumeric: 'tabular-nums', color: 'var(--accent)',
      }}>{value}</span>
      {unit ? <span style={{ fontFamily: 'var(--font-mono)', fontSize: 12, color: 'var(--ink-muted)' }}>{unit}</span> : null}
    </div>
    <Sparkbar values={spark || []} height={22} highlight={highlight} />
  </Card>;
}

/** One of the three "worth a look" cards. Each states a change, shows the
    shape of it, and goes somewhere. */
function Lead({ tone, kind, delta, children, footer, chart, onClick }) {
  const [hover, setHover] = React.useState(false);
  return <div onClick={onClick} role="button" tabIndex={0}
    onKeyDown={(e) => { if (e.key === 'Enter') onClick(); }}
    onMouseEnter={() => setHover(true)} onMouseLeave={() => setHover(false)}
    style={{
      background: hover ? 'var(--bg-sunken)' : 'var(--bg-raised)',
      border: '1px solid var(--hairline)', borderRadius: 'var(--radius-card)',
      padding: '14px 16px', cursor: 'pointer', display: 'flex', flexDirection: 'column', gap: 10,
    }}>
    <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
      <span style={{
        fontSize: 11, textTransform: 'uppercase', letterSpacing: 'var(--tracking-caps)',
        color: tone, fontWeight: 500,
      }}>{kind}</span>
      <span style={{
        marginLeft: 'auto', fontFamily: 'var(--font-mono)', fontSize: 12,
        fontVariantNumeric: 'tabular-nums', color: tone,
      }}>{delta}</span>
    </div>
    <div style={{ fontSize: 13, lineHeight: '19px', color: 'var(--ink)', textWrap: 'pretty' }}>{children}</div>
    {chart}
    <div style={{ fontSize: 12, color: 'var(--ink-muted)' }}>{footer}</div>
  </div>;
}

const M = ({ children, color }) => <span style={{ fontFamily: 'var(--font-mono)', fontSize: 12, color }}>{children}</span>;

export function OverviewScreen({ go }) {
  const [range, setRange] = React.useState('24h');
  const window = React.useMemo(() => windowOf(range), [range]);

  // The window is split in half so "since yesterday" is a comparison the
  // server computes, not a second request: buckets 0..n/2 are the previous
  // period and n/2..n the current one.
  const series = useRead(
    (signal) => api.series({ since: Math.round(window.sinceNs), until: Math.round(window.untilNs), buckets: 48 }, signal),
    [window.sinceNs, window.untilNs],
    { skip: !window.sinceNs },
  );
  const failures = useRead(
    (signal) => api.failures({ since: Math.round(window.sinceNs), until: Math.round(window.untilNs), limit: 5 }, signal),
    [window.sinceNs, window.untilNs],
    { skip: !window.sinceNs },
  );
  const models = useRead(
    (signal) => api.llmStats({ group_by: 'model', since: Math.round(window.sinceNs), limit: 6 }, signal),
    [window.sinceNs],
    { skip: !window.sinceNs },
  );
  const sessions = useRead((signal) => api.sessions({ limit: 5 }, signal), []);
  const metrics = useRead((signal) => api.metrics(signal), []);

  const buckets = series.data?.buckets || [];
  const half = Math.floor(buckets.length / 2);
  const sum = (list, key) => list.reduce((total, b) => total + (b[key] || 0), 0);
  const now = buckets.slice(half);
  const before = buckets.slice(0, half);

  const spans = sum(now, 'spans');
  const errors = sum(now, 'errors');
  const cost = sum(now, 'cost_usd');
  const tokens = sum(now, 'total_tokens');
  const p95 = Math.max(...now.map((b) => b.p95_ns || 0), 0);
  const p95Before = Math.max(...before.map((b) => b.p95_ns || 0), 0);

  const worstFailure = failures.data?.groups?.[0];
  const totalFailures = (failures.data?.groups || []).reduce((t, g) => t + g.count, 0);
  const topModel = models.data?.rows?.[0];
  const modelCost = (models.data?.rows || []).reduce((t, r) => t + r.cost_usd, 0);

  const loading = series.loading || failures.loading;
  const error = series.error || failures.error;

  return <div style={{ display: 'grid', gap: 16, maxWidth: 1560 }}>
    <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
      {RANGES.filter((r) => r.ms).map((r) => (
        <Chip key={r.id} mono active={range === r.id} onClick={() => setRange(r.id)}>{r.label}</Chip>
      ))}
      <span style={{ fontSize: 12, color: 'var(--ink-faint)', fontFamily: 'var(--font-mono)', marginLeft: 4 }}>
        {fmtWindowLabel(window.sinceNs, window.untilNs)}
      </span>
      <span style={{ marginLeft: 'auto', fontSize: 12, color: 'var(--ink-muted)', display: 'flex', alignItems: 'center', gap: 6 }}>
        <LiveDot />live
      </span>
    </div>

    <LoadingBar active={loading} />
    {error ? <ErrorState what={error.what} next={error.next} onRetry={series.reload} /> : null}

    <div style={{ display: 'grid', gridTemplateColumns: 'repeat(5,1fr)', gap: 12 }}>
      <Tile label="spans" value={fmtCompact(spans)}
        delta={fmtDelta(spans, sum(before, 'spans'))}
        spark={now.map((b) => b.spans)} />
      <Tile label="errors" value={fmtNum(errors)} unit={spans ? fmtPercent(errors, spans) : ''}
        delta={fmtDelta(errors, sum(before, 'errors'))}
        deltaTone={errors > sum(before, 'errors') ? 'error' : undefined}
        spark={now.map((b) => b.errors)} />
      <Tile label="p95 latency" value={p95 ? fmtDurationNs(p95) : '—'}
        delta={fmtDelta(p95, p95Before)}
        deltaTone={p95 > p95Before * 1.2 ? 'warn' : undefined}
        spark={now.map((b) => b.p95_ns)} />
      <Tile label="spend" value={cost.toFixed(2)} unit="USD"
        delta={fmtDelta(cost, sum(before, 'cost_usd'))}
        spark={now.map((b) => b.cost_usd)} />
      <Tile label="tokens" value={fmtCompact(tokens)}
        delta={fmtDelta(tokens, sum(before, 'total_tokens'))}
        spark={now.map((b) => b.total_tokens)} />
    </div>

    <div>
      <div style={{ display: 'flex', alignItems: 'baseline', gap: 10, marginBottom: 9 }}>
        <Eyebrow>Worth a look</Eyebrow>
        <span style={{ fontSize: 12, color: 'var(--ink-faint)' }}>
          ranked by how much they moved against the previous {RANGES.find((r) => r.id === range)?.label}
        </span>
      </div>
      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(3,1fr)', gap: 12 }}>
        <Lead tone="var(--warn)" kind="latency" delta={fmtDelta(p95, p95Before) || '—'}
          onClick={() => go(['latency'])}
          chart={<div style={{ position: 'relative', height: 38, background: 'var(--bg-sunken)', borderRadius: 3, overflow: 'hidden' }}>
            <Sparkbar values={buckets.map((b) => b.p95_ns)} height={38} gap={0}
              highlight={(_, i) => i >= half} />
            <div style={{
              position: 'absolute', left: '50%', top: 0, bottom: 0, width: 1,
              background: 'repeating-linear-gradient(var(--ink) 0 3px,transparent 3px 6px)',
            }} />
          </div>}
          footer={`${fmtNum(spans)} spans in window · open the distribution →`}>
          {p95 ? <>p95 moved from <M>{fmtDurationNs(p95Before)}</M> to <M color="var(--accent)">{fmtDurationNs(p95)}</M> across the window.</>
            : 'No spans in this window yet.'}
        </Lead>

        <Lead tone="var(--error)" kind="failures" delta={totalFailures ? `${fmtNum(totalFailures)} total` : 'none'}
          onClick={() => go(['failures'])}
          chart={<StackedSparkbar buckets={now} height={38} />}
          footer={worstFailure
            ? `first seen ${fmtClockNs(worstFailure.first_seen_ns)} · last ${fmtAgo(worstFailure.last_seen_ns)} ago →`
            : 'nothing is failing in this window'}>
          {worstFailure
            ? <>One signature is {fmtPercent(worstFailure.count, totalFailures)} of errors:{' '}
              <M>{worstFailure.service} · {worstFailure.name} · {worstFailure.status}</M>.</>
            : 'No errors in this window.'}
        </Lead>

        <Lead tone="var(--accent-hover)" kind="spend"
          delta={topModel && modelCost ? fmtPercent(topModel.cost_usd, modelCost) : '—'}
          onClick={() => go(['analytics'])}
          chart={<div style={{ display: 'flex', alignItems: 'flex-end', gap: 6, height: 38 }}>
            {(models.data?.rows || []).slice(0, 5).map((row, i) => {
              const max = Math.max(...(models.data?.rows || []).map((r) => r.cost_usd), 0);
              return <div key={row.key} style={{ flex: 1, display: 'flex', flexDirection: 'column', justifyContent: 'flex-end', height: '100%' }}>
                <div style={{
                  height: max ? Math.max(2, (row.cost_usd / max) * 100) + '%' : '2px',
                  background: i === 0 ? 'var(--accent)' : `var(--measure-${Math.max(1, 3 - i)})`,
                  borderRadius: '1.5px 1.5px 0 0', minHeight: 2,
                }} />
              </div>;
            })}
          </div>}
          footer={`$${fmtCost(cost)} in window · $${spans ? (cost / spans).toFixed(6) : '0'} per span →`}>
          {topModel
            ? <><M>{topModel.key}</M> took {fmtPercent(topModel.cost_usd, modelCost)} of spend on{' '}
              {fmtNum(topModel.llm_calls)} calls.</>
            : 'No LLM calls in this window.'}
        </Lead>
      </div>
    </div>

    <div style={{ display: 'grid', gridTemplateColumns: '1.15fr 1fr', gap: 12 }}>
      <Card>
        <div style={{ display: 'flex', alignItems: 'baseline', marginBottom: 10 }}>
          <Eyebrow>Server, in its own words</Eyebrow>
          <span onClick={() => go(['server'])} role="link" tabIndex={0}
            onKeyDown={(e) => { if (e.key === 'Enter') go(['server']); }}
            style={{ marginLeft: 'auto', fontSize: 12, color: 'var(--accent)', cursor: 'pointer' }}>Open →</span>
        </div>
        {metrics.data ? <>
          <div style={{ fontSize: 13, lineHeight: '21px', color: 'var(--ink)', marginBottom: 12, textWrap: 'pretty' }}>
            Answered <M color="var(--accent)">{fmtNum(metrics.data.requests.total)}</M> requests at{' '}
            <M color="var(--accent)">p95 {fmtDurationNs(metrics.data.requests.p95_ns)}</M>. Admitted{' '}
            <M color="var(--accent)">{fmtNum(metrics.data.ingest.spans_admitted)}</M> spans. Durability is{' '}
            <M>{metrics.data.durability || 'wal'}</M> — an acknowledged write survives a kill&#8209;9, a panic,
            or an OS crash.
          </div>
          <div style={{ display: 'grid', gridTemplateColumns: 'repeat(4,1fr)', gap: 10 }}>
            {[
              ['search p95', fmtDurationNs(metrics.data.by_class.search.p95_ns)],
              ['lookup p95', fmtDurationNs(metrics.data.by_class.lookup.p95_ns)],
              ['stats p95', fmtDurationNs(metrics.data.by_class.stats.p95_ns)],
              ['5xx', fmtNum(metrics.data.requests.responses_5xx)],
            ].map(([label, value]) => <div key={label} style={{ borderLeft: '2px solid var(--hairline)', paddingLeft: 9 }}>
              <div style={{ fontSize: 11, color: 'var(--ink-muted)', marginBottom: 2 }}>{label}</div>
              <div style={{
                fontFamily: 'var(--font-mono)', fontSize: 14,
                fontVariantNumeric: 'tabular-nums', color: 'var(--ink)',
              }}>{value}</div>
            </div>)}
          </div>
        </> : <Skeleton height={92} />}
      </Card>

      <Card>
        <div style={{ display: 'flex', alignItems: 'baseline', marginBottom: 8 }}>
          <Eyebrow>Recent sessions</Eyebrow>
          <span onClick={() => go(['sessions'])} role="link" tabIndex={0}
            onKeyDown={(e) => { if (e.key === 'Enter') go(['sessions']); }}
            style={{ marginLeft: 'auto', fontSize: 12, color: 'var(--accent)', cursor: 'pointer' }}>All sessions →</span>
        </div>
        {(sessions.data?.sessions || []).map((s) => <SessionRow key={s.session_id} session={s}
          onClick={() => go(['sessions', s.session_id])} />)}
        {sessions.data && !sessions.data.sessions?.length
          ? <div style={{ fontSize: 13, color: 'var(--ink-muted)', padding: '6px 0' }}>No sessions yet.</div>
          : null}
      </Card>
    </div>
  </div>;
}

function SessionRow({ session, onClick }) {
  const [hover, setHover] = React.useState(false);
  return <div onClick={onClick} role="link" tabIndex={0}
    onKeyDown={(e) => { if (e.key === 'Enter') onClick(); }}
    onMouseEnter={() => setHover(true)} onMouseLeave={() => setHover(false)}
    style={{
      display: 'flex', alignItems: 'center', gap: 10, padding: '5px 4px',
      borderBottom: '1px solid var(--hairline)', cursor: 'pointer',
      background: hover ? 'var(--bg-sunken)' : 'transparent',
    }}>
    <span style={{
      fontFamily: 'var(--font-mono)', fontSize: 12, color: 'var(--ink)', width: 96,
      overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
    }}>{session.session_id}</span>
    <span style={{
      fontFamily: 'var(--font-mono)', fontSize: 12, fontVariantNumeric: 'tabular-nums',
      color: 'var(--ink-muted)', width: 60, textAlign: 'right',
    }}>{session.trace_count} turns</span>
    <span style={{
      fontFamily: 'var(--font-mono)', fontSize: 12, fontVariantNumeric: 'tabular-nums',
      color: 'var(--accent)', width: 64, textAlign: 'right',
    }}>{fmtCost(session.cost_usd)}</span>
    <span style={{
      fontFamily: 'var(--font-mono)', fontSize: 12, fontVariantNumeric: 'tabular-nums',
      color: session.error_count ? 'var(--error)' : 'var(--ink-faint)', width: 30, textAlign: 'right',
    }}>{session.error_count}</span>
    <span style={{ fontSize: 11, color: 'var(--ink-faint)', width: 44, textAlign: 'right' }}>
      {fmtAgo(session.last_end_ns)}
    </span>
  </div>;
}
