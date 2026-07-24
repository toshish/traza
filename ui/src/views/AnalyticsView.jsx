import React from 'react';
import { api } from '../lib/api.js';
import { fmtNum, fmtCost, fmtAvgLatency } from '../lib/format.js';
import { Section } from '../components/Section.jsx';
import { Button } from '../components/primitives/Button.jsx';
import { Select } from '../components/primitives/Select.jsx';
import { Tabs } from '../components/primitives/Tabs.jsx';
import { DataTable } from '../components/data/DataTable.jsx';
import { StatTile } from '../components/data/StatTile.jsx';
import { BarChart } from '../components/charts/BarChart.jsx';
import { LineChart } from '../components/charts/LineChart.jsx';
import { EmptyState } from '../components/feedback/EmptyState.jsx';
import { ErrorState } from '../components/feedback/ErrorState.jsx';
import { LoadingBar } from '../components/feedback/LoadingBar.jsx';

const RANGES = [
  { id: 'all', label: 'all time', sinceNs: () => undefined },
  { id: '1h', label: 'last hour', sinceNs: () => (Date.now() - 3600e3) * 1e6 },
  { id: '24h', label: 'last 24 h', sinceNs: () => (Date.now() - 86400e3) * 1e6 },
  { id: '7d', label: 'last 7 days', sinceNs: () => (Date.now() - 7 * 86400e3) * 1e6 },
];

const CHART_KEYS = 12; // top rows charted; the table shows everything

/** Token/cost analytics over /v1/stats/llm, grouped how you ask. */
export function AnalyticsView() {
  const [groupBy, setGroupBy] = React.useState('model');
  const [range, setRange] = React.useState('all');
  const [rows, setRows] = React.useState(null);
  const [error, setError] = React.useState(null);
  const [loading, setLoading] = React.useState(true);

  const fetchRows = React.useCallback(async () => {
    setLoading(true); setError(null);
    const since = RANGES.find((r) => r.id === range).sinceNs();
    try {
      const data = await api.llmStats({ group_by: groupBy, since: since && Math.round(since) });
      setRows(data.rows || []);
    } catch (e) {
      setError(e); setRows(null);
    } finally {
      setLoading(false);
    }
  }, [groupBy, range]);
  React.useEffect(() => { fetchRows(); }, [fetchRows]);

  const totals = (rows || []).reduce((acc, r) => ({
    calls: acc.calls + r.llm_calls,
    tokens: acc.tokens + r.total_tokens,
    cost: acc.cost + r.cost_usd,
    errors: acc.errors + r.error_count,
    durationNs: acc.durationNs + r.llm_duration_ns,
  }), { calls: 0, tokens: 0, cost: 0, errors: 0, durationNs: 0 });

  // Day grouping reads as a series over time; the others compare keys.
  const isDaily = groupBy === 'day';
  const charted = (rows || []).slice(0, CHART_KEYS);
  const daily = isDaily ? [...charted].sort((a, b) => (a.key < b.key ? -1 : 1)) : [];

  return <Section title="LLM analytics" action={<Button variant="ghost" size="sm" onClick={fetchRows}>Refresh</Button>}>
    <div style={{ display: 'flex', gap: 12, alignItems: 'center', marginBottom: 12, flexWrap: 'wrap' }}>
      <span style={{ fontSize: 'var(--text-12)', color: 'var(--ink-muted)' }}>group by</span>
      <Select size="sm" value={groupBy} onChange={setGroupBy}
        options={[{ value: 'model', label: 'model' }, { value: 'service', label: 'service' }, { value: 'session', label: 'session' }, { value: 'day', label: 'day' }]} />
      <Tabs tabs={RANGES.map((r) => ({ id: r.id, label: r.label }))} active={range} onChange={setRange} style={{ borderBottom: 'none' }} />
    </div>
    <LoadingBar active={loading} style={{ marginBottom: 8 }} />
    {error ? <ErrorState what={error.what} next={error.next} /> : null}
    {rows && !rows.length ? <EmptyState
      message={<span>No LLM calls in this window. Traza recognizes spans carrying <code>llm.*</code> usage attributes:</span>}
      command={'"attributes": {"llm.model": "…", "llm.prompt_tokens": 412,\n               "llm.completion_tokens": 88, "llm.cost_usd": 0.0042}'} /> : null}
    {rows && rows.length ? <>
      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(150px, 1fr))', gap: 12, marginBottom: 16 }}>
        <StatTile label="LLM calls" value={fmtNum(totals.calls)} />
        <StatTile label="total tokens" value={fmtNum(totals.tokens)} />
        <StatTile label="cost" value={fmtCost(totals.cost)} unit="USD" />
        <StatTile label="average latency" value={fmtAvgLatency(totals.durationNs, totals.calls) || '—'} />
        <StatTile label="errors" value={fmtNum(totals.errors)} />
      </div>
      <div style={{ display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 16, marginBottom: 16 }}>
        <div>
          <div style={{ fontSize: 'var(--text-12)', color: 'var(--ink-muted)', marginBottom: 8 }}>tokens by {groupBy} — accent prompt, compare completion</div>
          {isDaily
            ? <LineChart height={120} values={daily.map((r) => r.total_tokens)} labels={daily.map((r) => r.key.slice(5))} />
            : <BarChart height={120} data={charted.map((r) => ({ label: r.key, value: r.prompt_tokens, compare: r.completion_tokens }))} />}
        </div>
        <div>
          <div style={{ fontSize: 'var(--text-12)', color: 'var(--ink-muted)', marginBottom: 8 }}>cost USD by {groupBy}</div>
          {isDaily
            ? <LineChart height={120} values={daily.map((r) => r.cost_usd)} labels={daily.map((r) => r.key.slice(5))} />
            : <BarChart height={120} formatValue={(v) => v.toFixed(4)} data={charted.map((r) => ({ label: r.key, value: r.cost_usd }))} />}
        </div>
      </div>
      <div style={{ overflowX: 'auto' }}>
        <DataTable density="dense" columns={[
          { key: 'key', label: groupBy, mono: true },
          { key: 'llm_calls', label: 'LLM calls', align: 'right', mono: true, render: (v) => fmtNum(v) },
          { key: 'prompt_tokens', label: 'prompt', align: 'right', mono: true, render: (v) => fmtNum(v) },
          { key: 'completion_tokens', label: 'completion', align: 'right', mono: true, render: (v) => fmtNum(v) },
          { key: 'total_tokens', label: 'total tokens', align: 'right', mono: true, render: (v) => fmtNum(v) },
          { key: 'cost_usd', label: 'cost USD', align: 'right', mono: true, render: (v) => fmtCost(v) },
          { key: 'lat', label: 'avg latency', align: 'right', mono: true, render: (_, r) => fmtAvgLatency(r.llm_duration_ns, r.llm_calls) || '—' },
          { key: 'error_count', label: 'errors', align: 'right', mono: true, render: (v) => <span style={{ color: v ? 'var(--error)' : 'var(--ink-faint)' }}>{v}</span> },
        ]} rows={rows} />
      </div>
    </> : null}
  </Section>;
}
