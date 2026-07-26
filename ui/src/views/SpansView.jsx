import React from 'react';
import { api } from '../lib/api.js';
import { fmtDurationNs, fmtTimeNs } from '../lib/format.js';
import { Section } from '../components/Section.jsx';
import { Button } from '../components/primitives/Button.jsx';
import { Input } from '../components/primitives/Input.jsx';
import { LoadMore } from '../components/primitives/LoadMore.jsx';
import { DataTable } from '../components/data/DataTable.jsx';
import { FilterBar } from '../components/trace/FilterBar.jsx';
import { EmptyState } from '../components/feedback/EmptyState.jsx';
import { ErrorState } from '../components/feedback/ErrorState.jsx';
import { LoadingBar } from '../components/feedback/LoadingBar.jsx';

// Built from the page's own origin so the copy stays literal-URL-free
// (the embedded page must reference no external URLs) and correct behind
// any host/port.
const EMPTY_COMMAND = `export OTEL_EXPORTER_OTLP_ENDPOINT=${window.location.origin}
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
export OTEL_EXPORTER_OTLP_COMPRESSION=none`;

const PAGE = 100;

function toNs(local) {
  const ms = new Date(local).getTime();
  return Number.isFinite(ms) ? String(ms * 1e6) : '';
}

/** Span search: content search plus the structured filter form, applied-filter
 * chips, and the results table. */
export function SpansView({ openTrace, sessionFilter, clearSessionFilter }) {
  const [form, setForm] = React.useState({ content: '', service: '', name: '', attrKey: '', attrValue: '', minMs: '', since: '', until: '' });
  const [applied, setApplied] = React.useState(form);
  const [limit, setLimit] = React.useState(PAGE);
  const [spans, setSpans] = React.useState(null);
  const [error, setError] = React.useState(null);
  const [loading, setLoading] = React.useState(false);
  const set = (key) => (value) => setForm((f) => ({ ...f, [key]: value }));

  const fetchSpans = React.useCallback(async (filters, effectiveLimit) => {
    setLoading(true); setError(null);
    const params = {
      // Word search over the span's text. Not substring, not a phrase --
      // "refund" finds "Refund the order" and not "refunds". See
      // /v1/spans?content= in the HTTP API guide.
      content: filters.content,
      service: filters.service, name: filters.name,
      min_duration_ms: filters.minMs,
      since: filters.since ? toNs(filters.since) : '',
      until: filters.until ? toNs(filters.until) : '',
      limit: effectiveLimit,
    };
    if (filters.attrKey) params['attr.' + filters.attrKey] = filters.attrValue;
    // The dedicated session filter unions every recognized session key, so a
    // mixed-convention session returns whole (see /v1/spans?session=).
    if (sessionFilter) params.session = sessionFilter;
    try {
      setSpans(await api.spans(params));
    } catch (e) {
      setError(e); setSpans(null);
    } finally {
      setLoading(false);
    }
  }, [sessionFilter]);

  React.useEffect(() => { fetchSpans(applied, limit); }, [fetchSpans]); // initial + session change

  const search = () => { setApplied(form); setLimit(PAGE); fetchSpans(form, PAGE); };
  const loadMore = () => { const next = limit + PAGE; setLimit(next); fetchSpans(applied, next); };
  const onKeyDown = (e) => { if (e.key === 'Enter') search(); };

  const chips = [
    applied.content && { field: 'content', value: applied.content },
    applied.service && { field: 'service', value: applied.service },
    applied.name && { field: 'name', value: applied.name },
    applied.attrKey && { field: 'attr.' + applied.attrKey, value: applied.attrValue },
    applied.minMs && { field: 'duration', op: '≥', value: applied.minMs + ' ms' },
    sessionFilter && { field: 'session', value: sessionFilter, session: true },
  ].filter(Boolean);

  return <Section title="Spans" action={<Button variant="ghost" size="sm" onClick={() => fetchSpans(applied, limit)}>Refresh</Button>}>
    {/* Content search gets the full width and the first position: it is the
        one filter a user reaches for without already knowing the schema. */}
    <div style={{ marginBottom: 6 }}>
      <Input size="sm" placeholder="search text in prompts, completions and events (words, not substrings)"
        title={'Finds spans containing every word given, anywhere in their text.\n'
          + 'Word matching, not substring: "refund" finds "Refund the order", not "refunds".\n'
          + 'Multiple words are ANDed, in any order.'}
        value={form.content} onChange={set('content')} onKeyDown={onKeyDown} style={{ width: '100%' }} />
    </div>
    <div style={{ display: 'grid', gridTemplateColumns: 'repeat(4, 1fr)', gap: 6, marginBottom: 6 }}>
      <Input size="sm" placeholder="service" value={form.service} onChange={set('service')} onKeyDown={onKeyDown} />
      <Input size="sm" placeholder="name" value={form.name} onChange={set('name')} onKeyDown={onKeyDown} />
      <Input size="sm" mono placeholder="attr key" value={form.attrKey} onChange={set('attrKey')} onKeyDown={onKeyDown} />
      <Input size="sm" mono placeholder="attr value" value={form.attrValue} onChange={set('attrValue')} onKeyDown={onKeyDown} />
      <Input size="sm" mono placeholder="min duration ms" value={form.minMs} onChange={set('minMs')} onKeyDown={onKeyDown} />
      <Input size="sm" mono type="datetime-local" title="since (local time)" value={form.since} onChange={set('since')} />
      <Input size="sm" mono type="datetime-local" title="until (local time)" value={form.until} onChange={set('until')} />
      <Button variant="primary" size="sm" onClick={search} style={{ justifyContent: 'center' }}>Search</Button>
    </div>
    {chips.length ? <FilterBar chips={chips} onRemoveChip={(c) => {
      if (c.session) { clearSessionFilter(); return; }
      const next = { ...applied };
      if (c.field === 'content') next.content = '';
      if (c.field === 'service') next.service = '';
      if (c.field === 'name') next.name = '';
      if (c.field.startsWith('attr.')) { next.attrKey = ''; next.attrValue = ''; }
      if (c.field === 'duration') next.minMs = '';
      setForm(next); setApplied(next); fetchSpans(next, limit);
    }} /> : null}
    <LoadingBar active={loading} style={{ marginBottom: 8 }} />
    {error ? <ErrorState what={error.what} next={error.next} /> : null}
    {spans && !spans.length && !error ? <EmptyState
      message="No spans match. If the store is empty, point an OTel SDK at this server:"
      command={EMPTY_COMMAND} /> : null}
    {spans && spans.length ? <>
      <div style={{ overflowX: 'auto' }}>
        <DataTable density="dense" onRowClick={(r) => openTrace(r.trace_id, r.span_id)} columns={[
          { key: 'start_time_ns', label: 'start (UTC)', mono: true, render: (v) => fmtTimeNs(v) },
          { key: 'service', label: 'service' },
          { key: 'name', label: 'name' },
          { key: 'trace_id', label: 'trace', mono: true, maxWidth: 140 },
          { key: 'dur', label: 'duration', align: 'right', mono: true, render: (_, r) => fmtDurationNs(r.end_time_ns - r.start_time_ns) },
          { key: 'status', label: 'status', render: (v) => <span style={{ color: v === 'error' ? 'var(--error)' : 'var(--ink-muted)' }}>{v || '—'}</span> },
        ]} rows={spans} />
      </div>
      <LoadMore shown={spans.length} onClick={loadMore} loading={loading} />
    </> : null}
  </Section>;
}
