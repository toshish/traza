import React from 'react';
import { api } from '../lib/api.js';
import { useRead, usePoll } from '../lib/route.js';
import { RANGES, windowOf } from '../lib/query.js';
import {
  durabilityMeans, fmtCompact, fmtCost, fmtDelta, fmtDurationNs, fmtNum, fmtPercent, fmtAgo,
  fmtClockNs, fmtWindowLabel,
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

/** How often a screen labelled "live" actually re-reads. */
const REFRESH_MS = 15000;

export function OverviewScreen({ go }) {
  const [range, setRange] = React.useState('24h');
  // The screen says "live", so it has to be. The window was resolved once at
  // mount and never again: every figure, spark and lead card was frozen at the
  // moment the tab opened, and the dot pulsed over it. `tick` re-resolves the
  // relative window, which re-runs every read below it.
  const [tick, setTick] = React.useState(0);
  usePoll(() => setTick((n) => n + 1), REFRESH_MS);
  // TWO full periods, not one window cut in half.
  //
  // Splitting the selected range down the middle meant "24h" showed the last
  // twelve hours against the twelve before them, under a label that said
  // "previous 24h" — and the failure and model cards queried the whole 24
  // hours, so three cards on one screen described three different spans of
  // time. `current` is the range the user picked; `previous` is the same
  // length immediately before it. One series request covers both, with the
  // midpoint landing exactly on the boundary, so a comparison is still one
  // round trip rather than two.
  // eslint-disable-next-line react-hooks/exhaustive-deps
  const current = React.useMemo(() => windowOf(range), [range, tick]);
  const previous = React.useMemo(() => {
    if (!current.sinceNs) return { sinceNs: null, untilNs: null };
    const span = current.untilNs - current.sinceNs;
    // `since` and `until` are both INCLUSIVE, so ending the previous period at
    // `current.sinceNs` counts a span landing exactly on the boundary in both
    // histograms — while the series, which buckets by offset, gives it only to
    // the current half. One nanosecond earlier makes the two periods disjoint
    // and the comparison a partition rather than an overlap.
    return { sinceNs: current.sinceNs - span, untilNs: current.sinceNs - 1 };
  }, [current]);

  // Even bucket count so the split is exact: the first half is `previous`,
  // the second is `current`, with no bucket straddling the boundary.
  const BUCKETS = 48;
  const series = useRead(
    (signal) => api.series({
      since: Math.round(previous.sinceNs),
      until: Math.round(current.untilNs),
      buckets: BUCKETS,
    }, signal),
    [previous.sinceNs, current.untilNs],
    { skip: !current.sinceNs },
  );
  // Failures and models describe the CURRENT period only — they sit beside the
  // tiles, and a card that silently covered twice the window the tiles did was
  // the inconsistency worth removing.
  const failures = useRead(
    (signal) => api.failures({
      since: Math.round(current.sinceNs), until: Math.round(current.untilNs), limit: 5,
    }, signal),
    [current.sinceNs, current.untilNs],
    { skip: !current.sinceNs },
  );
  const models = useRead(
    (signal) => api.llmStats({
      group_by: 'model',
      since: Math.round(current.sinceNs), until: Math.round(current.untilNs), limit: 6,
    }, signal),
    [current.sinceNs, current.untilNs],
    { skip: !current.sinceNs },
  );
  // p95 comes from a duration histogram PER PERIOD, not from the series.
  //
  // `max(bucket.p95)` is not the p95 of a period — it is the worst bucket's
  // p95, which a single sparse slow bucket drags into seconds while the true
  // period p95 sits in milliseconds. There is no way to combine per-bucket
  // percentiles into a period percentile; the distribution has to be folded
  // over the whole period, which is exactly what /v1/stats/duration does.
  const durationNow = useRead(
    (signal) => api.duration({
      since: Math.round(current.sinceNs), until: Math.round(current.untilNs),
    }, signal),
    [current.sinceNs, current.untilNs],
    { skip: !current.sinceNs },
  );
  const durationBefore = useRead(
    (signal) => api.duration({
      since: Math.round(previous.sinceNs), until: Math.round(previous.untilNs),
    }, signal),
    [previous.sinceNs, previous.untilNs],
    { skip: !previous.sinceNs },
  );
  // These two are window-independent, but they are still on a screen labelled
  // live, so they follow the tick like everything else.
  const sessions = useRead((signal) => api.sessions({ limit: 5 }, signal), [tick]);
  const metrics = useRead((signal) => api.metrics(signal), [tick]);

  const buckets = series.data?.buckets || [];
  const half = Math.floor(buckets.length / 2);
  const sum = (list, key) => list.reduce((total, b) => total + (b[key] || 0), 0);
  const now = buckets.slice(half);
  const before = buckets.slice(0, half);

  const spans = sum(now, 'spans');
  const errors = sum(now, 'errors');
  const cost = sum(now, 'cost_usd');
  const tokens = sum(now, 'total_tokens');
  const p95 = durationNow.data?.p95_ns ?? 0;
  const p95Before = durationBefore.data?.p95_ns ?? 0;

  const worstFailure = failures.data?.groups?.[0];
  // The server's own count of every matching span, not the sum of the five
  // groups it returned. Summing a truncated page made the top signature's
  // share read as a far larger fraction of failures than it was.
  const totalFailures = failures.data?.total ?? 0;
  const topModel = models.data?.rows?.[0];
  // Total spend from the SERIES, which covers every model, not the subtotal of
  // the six rows the limit returned. Summing a truncated list inflates the top
  // model's share by exactly what was left out — and the more models exist,
  // the more confident the wrong number looks.
  const modelCost = cost;

  // Every read that feeds a figure on this screen. `durationNow` supplies the
  // p95 tile and `models` the spend card, and both were outside this: after a
  // range change the series could finish first and paint new volume beside the
  // previous range's p95 with no loading bar, and a failed histogram silently
  // became an em dash.
  const reads = [series, failures, models, durationNow, durationBefore, sessions, metrics];
  const loading = reads.some((read) => read.loading);
  const error = reads.map((read) => read.error).find(Boolean);
  const reload = () => reads.forEach((read) => read.reload());

  return <div style={{ display: 'grid', gap: 16, maxWidth: 1560 }}>
    <div style={{ display: 'flex', alignItems: 'center', gap: 10 }}>
      {RANGES.filter((r) => r.ms).map((r) => (
        <Chip key={r.id} mono active={range === r.id} onClick={() => setRange(r.id)}>{r.label}</Chip>
      ))}
      <span style={{ fontSize: 12, color: 'var(--ink-faint)', fontFamily: 'var(--font-mono)', marginLeft: 4 }}>
        {fmtWindowLabel(current.sinceNs, current.untilNs)}
      </span>
      <span style={{ marginLeft: 'auto', fontSize: 12, color: 'var(--ink-muted)', display: 'flex', alignItems: 'center', gap: 6 }}>
        <LiveDot />live
      </span>
    </div>

    <LoadingBar active={loading} />
    {error ? <ErrorState what={error.what} next={error.next} onRetry={reload} /> : null}

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
          {p95 ? <>p95 was <M>{fmtDurationNs(p95Before)}</M> in the previous period and{' '}<M color="var(--accent)">{fmtDurationNs(p95)}</M> in this one.</>
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
            <M>{metrics.data.durability || '—'}</M> — {durabilityMeans(metrics.data.durability)}.
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
