import React from 'react';
import { api } from '../lib/api.js';
import { useRead } from '../lib/route.js';
import { RANGES, windowOf } from '../lib/query.js';
import { fmtCompact, fmtCost, fmtCostProvenance, fmtDurationNs, fmtNum, fmtPercent } from '../lib/format.js';
import { Card, Chip, Eyebrow, ErrorState, EmptyState, LoadingBar } from '../components/primitives/Chrome.jsx';
import { CategoryBars, Sparkbar, TimeAxis } from '../components/charts/Marks.jsx';

// Cost, tokens, latency, throughput and efficiency over any grouping. The old
// screen charted tokens and cost and nothing else, and reported LLM latency as
// a mean — which hides exactly the tail anyone is looking for.
//
// Efficiency is the addition that matters: a total tells you what you spent,
// cost-per-answer tells you whether it was worth it.

const MEASURES = [
  { id: 'cost', label: 'cost', unit: 'USD', of: (r) => r.cost_usd, format: fmtCost },
  { id: 'tokens', label: 'tokens', of: (r) => r.total_tokens, format: fmtCompact },
  { id: 'calls', label: 'calls', of: (r) => r.llm_calls, format: fmtNum },
  { id: 'latency', label: 'latency', of: (r) => (r.llm_calls ? r.llm_duration_ns / r.llm_calls : 0), format: fmtDurationNs },
  { id: 'errors', label: 'errors', of: (r) => r.error_count, format: fmtNum },
  { id: 'per_call', label: 'cost / call', unit: 'USD', of: (r) => (r.llm_calls ? r.cost_usd / r.llm_calls : 0), format: (v) => v.toFixed(6) },
  { id: 'per_1k', label: 'cost / 1k tokens', unit: 'USD', of: (r) => (r.total_tokens ? (r.cost_usd / r.total_tokens) * 1000 : 0), format: (v) => v.toFixed(6) },
];

const GROUPS = ['model', 'provider', 'service', 'session', 'day'];

export function AnalyticsScreen({ go }) {
  const [groupBy, setGroupBy] = React.useState('model');
  const [measure, setMeasure] = React.useState('cost');
  const [range, setRange] = React.useState('24h');
  const window = React.useMemo(() => windowOf(range), [range]);

  const stats = useRead(
    (signal) => api.llmStats({
      group_by: groupBy,
      since: window.sinceNs ? Math.round(window.sinceNs) : undefined,
      until: window.untilNs ? Math.round(window.untilNs) : undefined,
      // A grouping the store can answer without bound — sessions on a large
      // store is thousands of rows — is truncated here rather than shipped
      // whole to be sliced in the browser.
      limit: 200,
    }, signal),
    [groupBy, window.sinceNs, window.untilNs],
  );
  const series = useRead(
    (signal) => api.series({ since: Math.round(window.sinceNs), until: Math.round(window.untilNs), buckets: 48 }, signal),
    [window.sinceNs, window.untilNs],
    { skip: !window.sinceNs },
  );

  const rows = stats.data?.rows || [];
  const active = MEASURES.find((m) => m.id === measure);
  const sorted = React.useMemo(
    () => [...rows].sort((a, b) => active.of(b) - active.of(a)),
    [rows, active],
  );
  const totals = rows.reduce((acc, r) => ({
    calls: acc.calls + r.llm_calls,
    tokens: acc.tokens + r.total_tokens,
    cost_usd: acc.cost_usd + r.cost_usd,
    cost_derived_usd: acc.cost_derived_usd + (r.cost_derived_usd || 0),
    cost_metered_calls: acc.cost_metered_calls + (r.cost_metered_calls || 0),
    cost_derived_calls: acc.cost_derived_calls + (r.cost_derived_calls || 0),
    cost_unpriced_calls: acc.cost_unpriced_calls + (r.cost_unpriced_calls || 0),
    errors: acc.errors + r.error_count,
    durationNs: acc.durationNs + r.llm_duration_ns,
    spans: acc.spans + r.spans,
  }), {
    calls: 0, tokens: 0, cost_usd: 0, cost_derived_usd: 0,
    cost_metered_calls: 0, cost_derived_calls: 0, cost_unpriced_calls: 0,
    errors: 0, durationNs: 0, spans: 0,
  });

  const buckets = series.data?.buckets || [];

  return <div style={{ display: 'grid', gap: 14, maxWidth: 1560 }}>
    <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
      <Eyebrow style={{ fontSize: 11 }}>group by</Eyebrow>
      {GROUPS.map((group) => <Chip key={group} active={groupBy === group} onClick={() => setGroupBy(group)}>{group}</Chip>)}
      <span style={{ marginLeft: 'auto', display: 'flex', gap: 6 }}>
        {RANGES.map((r) => <Chip key={r.id} mono active={range === r.id} onClick={() => setRange(r.id)}>{r.label}</Chip>)}
      </span>
    </div>

    <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
      <Eyebrow style={{ fontSize: 11 }}>measure</Eyebrow>
      {MEASURES.map((m) => <Chip key={m.id} active={measure === m.id} onClick={() => setMeasure(m.id)}>{m.label}</Chip>)}
    </div>

    <LoadingBar active={stats.loading} />
    {stats.error ? <ErrorState what={stats.error.what} next={stats.error.next} onRetry={stats.reload} /> : null}
    {stats.data && !rows.length ? <EmptyState
      message={<span>No LLM calls in this window. Traza recognizes the OpenLLMetry / OTel GenAI
        conventions (<code>gen_ai.*</code>, <code>llm.usage.*</code>) and its native <code>llm.*</code> shorthand:</span>}
      command={'"attributes": {"gen_ai.system": "openai", "gen_ai.request.model": "gpt-4o",\n               "gen_ai.usage.prompt_tokens": 412, "gen_ai.usage.completion_tokens": 88}'} /> : null}

    {rows.length ? <>
      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(6,1fr)', gap: 12 }}>
        {[
          ['calls', fmtCompact(totals.calls)],
          ['tokens', fmtCompact(totals.tokens)],
          ['cost', fmtCostProvenance(totals).text, 'USD'],
          ['mean latency', totals.calls ? fmtDurationNs(totals.durationNs / totals.calls) : '—'],
          ['cost / call', totals.calls
            ? (fmtCostProvenance(totals).estimated ? '~' : '')
              + (totals.cost_usd / totals.calls).toFixed(6)
            : '—', 'USD'],
          ['error rate', totals.spans ? fmtPercent(totals.errors, totals.spans) : '0%'],
        ].map(([label, value, unit]) => <Card key={label} pad="12px 14px">
          <div style={{ fontSize: 12, color: 'var(--ink-muted)', marginBottom: 5 }}>{label}</div>
          <div style={{ display: 'flex', alignItems: 'baseline', gap: 5 }}>
            <span style={{
              fontFamily: 'var(--font-mono)', fontSize: 20, lineHeight: '28px', fontWeight: 500,
              fontVariantNumeric: 'tabular-nums', color: 'var(--accent)',
            }}>{value}</span>
            {unit ? <span style={{ fontFamily: 'var(--font-mono)', fontSize: 12, color: 'var(--ink-muted)' }}>{unit}</span> : null}
          </div>
        </Card>)}
      </div>

      <div style={{ display: 'grid', gridTemplateColumns: '1.4fr 1fr', gap: 12 }}>
        <Card>
          <div style={{ display: 'flex', alignItems: 'baseline', marginBottom: 12 }}>
            <Eyebrow>{active.label} by {groupBy}</Eyebrow>
            <span style={{ marginLeft: 10, fontSize: 12, color: 'var(--ink-faint)' }}>top 12</span>
          </div>
          <CategoryBars rows={sorted.slice(0, 12)} valueOf={active.of}
            labelOf={(r) => r.key} format={active.format} />
        </Card>
        <Card>
          <div style={{ display: 'flex', alignItems: 'baseline', marginBottom: 12 }}>
            <Eyebrow>spend over time</Eyebrow>
          </div>
          <Sparkbar values={buckets.map((b) => b.cost_usd)} height={150} />
          {series.data ? <TimeAxis sinceNs={series.data.since_ns} untilNs={series.data.until_ns} /> : null}
        </Card>
      </div>

      <Card pad="0" style={{ overflow: 'hidden' }}>
        <div style={{
          display: 'grid', gridTemplateColumns: '1fr 84px 92px 100px 96px 96px 104px 76px',
          background: 'var(--bg-sunken)', borderBottom: '1px solid var(--hairline)',
        }}>
          {[groupBy, 'calls', 'prompt', 'completion', 'tokens', 'cost USD', 'mean latency', 'errors']
            .map((label, i) => <div key={label} style={{
              padding: '6px 10px', fontSize: 12, fontWeight: 500, color: 'var(--ink-muted)',
              textAlign: i === 0 ? 'left' : 'right', whiteSpace: 'nowrap',
            }}>{label}</div>)}
        </div>
        {sorted.map((row) => <div key={row.key}
          onClick={groupBy === 'session' ? () => go(['sessions', row.key]) : undefined}
          style={{
            display: 'grid', gridTemplateColumns: '1fr 84px 92px 100px 96px 96px 104px 76px',
            alignItems: 'center', borderBottom: '1px solid var(--hairline)',
            minHeight: 'var(--row-h)', cursor: groupBy === 'session' ? 'pointer' : 'default',
          }}>
          <div style={{
            padding: 'var(--row-py) 10px', fontFamily: 'var(--font-mono)', fontSize: 12,
            color: 'var(--ink)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
          }}>{row.key}</div>
          <Num>{fmtNum(row.llm_calls)}</Num>
          <Num muted>{fmtNum(row.prompt_tokens)}</Num>
          <Num muted>{fmtNum(row.completion_tokens)}</Num>
          <Num>{fmtNum(row.total_tokens)}</Num>
          <CostCell row={row} />
          <Num muted>{row.llm_calls ? fmtDurationNs(row.llm_duration_ns / row.llm_calls) : '—'}</Num>
          <Num tone={row.error_count ? 'var(--error)' : 'var(--ink-faint)'}>{row.error_count}</Num>
        </div>)}
      </Card>
    </> : null}
  </div>;
}

function Num({ children, muted, accent, tone }) {
  return <div style={{
    padding: 'var(--row-py) 10px', fontFamily: 'var(--font-mono)', fontSize: 12,
    fontVariantNumeric: 'tabular-nums', textAlign: 'right', whiteSpace: 'nowrap',
    color: tone || (accent ? 'var(--accent)' : muted ? 'var(--ink-muted)' : 'var(--ink)'),
  }}>{children}</div>;
}

/** A cost cell that marks a figure the pricing table worked out. */
function CostCell({ row }) {
  const cost = fmtCostProvenance(row);
  return <div title={cost.title} style={{
    padding: 'var(--row-py) 10px', fontFamily: 'var(--font-mono)', fontSize: 12,
    fontVariantNumeric: 'tabular-nums', textAlign: 'right',
    color: cost.estimated ? 'var(--ink-muted)' : 'var(--accent)',
  }}>{cost.text}</div>;
}
