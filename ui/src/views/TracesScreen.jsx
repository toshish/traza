import React from 'react';
import { api } from '../lib/api.js';
import { useRead, useKeys, useStored, navigate } from '../lib/route.js';
import {
  RANGES,
  fromHash,
  toHash,
  toParams,
  toCurl,
  predicate,
  opsFor,
  windowOf,
} from '../lib/query.js';
import { llmUsage } from '../lib/spans.js';
import { fmtClockNs, fmtCost, fmtDurationNs, fmtNum, fmtPercent, fmtWindowLabel } from '../lib/format.js';
import { Card, Chip, Eyebrow, ErrorState, EmptyState, Kbd, LoadingBar } from '../components/primitives/Chrome.jsx';
import { VolumeBrush, ShareBar } from '../components/charts/Marks.jsx';

// Span search, rebuilt around the query the API can actually answer. The old
// form had six boxes and reached about 60% of the parameter surface; this one
// is a predicate list, so exclusions, numeric bounds and repeated conditions
// are expressible — and the whole query lives in the URL, so it is shareable.

const COLUMNS = [
  { key: 'start', label: 'start', width: 112, sort: 'start' },
  { key: 'service', label: 'service', width: 116 },
  { key: 'name', label: 'name', width: 176 },
  { key: 'trace', label: 'trace', width: 92 },
  { key: 'bar', label: '', width: '1fr' },
  // 86px, not 78: "412.00 ms" is nine tabular glyphs plus padding, and the
  // narrower column clipped every millisecond-scale duration to an ellipsis.
  { key: 'duration', label: 'duration', width: 86, align: 'right', sort: 'duration' },
  { key: 'tokens', label: 'tokens', width: 74, align: 'right' },
  { key: 'cost', label: 'cost', width: 78, align: 'right' },
  { key: 'status', label: 'status', width: 62 },
];
const GRID = COLUMNS.map((c) => (typeof c.width === 'number' ? c.width + 'px' : c.width)).join(' ');

/** One predicate row: field, operator, value, remove. */
function PredicateRow({ pred, onChange, onRemove }) {
  const ops = opsFor(pred.field);
  const control = {
    padding: '3px 8px', border: '1px solid var(--hairline)', borderRadius: 'var(--radius-control)',
    background: 'var(--bg)', fontFamily: 'var(--font-mono)', fontSize: 12, color: 'var(--ink)',
    outline: 'none', minWidth: 0,
  };
  return <div style={{ display: 'grid', gridTemplateColumns: '300px 62px 1fr 24px', gap: 6, alignItems: 'center' }}>
    <input value={pred.field} aria-label="Field"
      onChange={(e) => {
        const field = e.target.value;
        const allowed = opsFor(field);
        onChange({ ...pred, field, op: allowed.includes(pred.op) ? pred.op : allowed[0] });
      }}
      list="traza-fields" placeholder="attr.llm.usage.total_tokens" style={control} />
    <select value={pred.op} aria-label="Operator"
      onChange={(e) => onChange({ ...pred, op: e.target.value })}
      style={{ ...control, textAlign: 'center', cursor: 'pointer', color: pred.op === '≠' ? 'var(--error)' : 'var(--ink)' }}>
      {ops.map((op) => <option key={op} value={op}>{op}</option>)}
    </select>
    <input value={pred.value} aria-label="Value"
      onChange={(e) => onChange({ ...pred, value: e.target.value })}
      placeholder="value" style={control} />
    <div onClick={onRemove} role="button" tabIndex={0} aria-label="Remove predicate"
      onKeyDown={(e) => { if (e.key === 'Enter') onRemove(); }}
      style={{ textAlign: 'center', color: 'var(--ink-faint)', cursor: 'pointer', fontSize: 13 }}>×</div>
  </div>;
}

/** What the query cost, stated rather than claimed. */
export function QueryCost({ cost, shown, total }) {
  if (!cost) return null;
  const pruned = cost.segments_examined
    ? fmtPercent(cost.segments_pruned, cost.segments_examined) : '0%';
  const read = cost.segments_examined - cost.segments_pruned;
  return <>
    <span style={{ fontFamily: 'var(--font-mono)', fontSize: 12, fontVariantNumeric: 'tabular-nums', color: 'var(--ink)' }}>
      {fmtNum(shown)} spans
    </span>
    <span style={{ color: 'var(--ink-faint)' }}>·</span>
    <span style={{ fontFamily: 'var(--font-mono)', fontSize: 12, fontVariantNumeric: 'tabular-nums', color: 'var(--accent)' }}>
      {fmtDurationNs(cost.elapsed_ns)}
    </span>
    <span style={{ color: 'var(--ink-faint)' }}>·</span>
    <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>
      {fmtNum(read)} of {fmtNum(cost.segments_examined)} segments read
    </span>
    <ShareBar part={read} whole={cost.segments_examined} />
    <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>{pruned} pruned by time range</span>
  </>;
}

export function TracesScreen({ go, params }) {
  const [query, setQuery] = React.useState(() => fromHash(params));
  const [pages, setPages] = React.useState([]);
  const [cursor, setCursor] = React.useState(null);
  const [selected, setSelected] = React.useState(0);
  const [views, setViews] = useStored('traza_views', []);
  const [applied, setApplied] = React.useState(() => fromHash(params));

  // The query lives in the URL so it can be sent to somebody. Replacing
  // rather than pushing keeps Back leaving the screen instead of walking
  // backwards through every predicate edit.
  //
  // The URL is the source of truth in BOTH directions. Writing only — which is
  // what a screen that reads the hash once at mount does — means a shared link
  // opened into an already-mounted screen shows the previous query, and
  // Back/Forward move the address bar without moving the page. `lastWritten`
  // is how the read side ignores the write side's own echo instead of the two
  // fighting each other.
  const lastWritten = React.useRef(null);
  React.useEffect(() => {
    const hash = new URLSearchParams(toHash(applied)).toString();
    lastWritten.current = hash;
    navigate(['traces'], toHash(applied), { replace: true });
  }, [applied]);

  React.useEffect(() => {
    const incoming = new URLSearchParams(toHash(fromHash(params))).toString();
    if (incoming === lastWritten.current) return;
    lastWritten.current = incoming;
    const next = fromHash(params);
    setQuery(next);
    setApplied(next);
  }, [params]);

  // "Now" is resolved ONCE per applied query and shared by both reads. Letting
  // each call its own `windowOf` meant the table and the volume chart were
  // asking about windows a few milliseconds apart — invisible, but it made the
  // chart's total and the table's cost disagree about which spans were in
  // scope, and a disagreement nobody can explain is worse than one nobody can
  // see.
  //
  // Named `timeWindow`, not `window`: the obvious name shadows the global for
  // the whole component, and the two places that reached for `window.prompt`
  // and `window.location` got `undefined` from the memo instead — silently,
  // because both were written defensively with `?.`.
  const timeWindow = React.useMemo(() => windowOf(applied.range), [applied]);
  const apiParams = React.useMemo(
    () => toParams(applied, {
      includeWindow: false,
      extra: timeWindow.sinceNs
        ? { since: Math.round(timeWindow.sinceNs), until: Math.round(timeWindow.untilNs) }
        : {},
    }),
    [applied, timeWindow],
  );

  const search = useRead((signal) => api.spans(apiParams, signal), [JSON.stringify(apiParams)]);
  const series = useRead(
    (signal) => api.series({ ...apiParams, limit: undefined, sort: undefined, buckets: 72 }, signal),
    [JSON.stringify(apiParams)],
    { skip: !timeWindow.sinceNs },
  );

  // A fresh search resets paging; "load more" appends one cursor page, so the
  // rows already on screen are never re-fetched.
  React.useEffect(() => { setPages([]); setCursor(search.data?.next_cursor || null); setSelected(0); }, [search.data]);

  const rows = React.useMemo(
    () => [...(search.data?.spans || []), ...pages.flat()],
    [search.data, pages],
  );

  const loadMore = async () => {
    if (!cursor) return;
    const page = await api.spans({ ...apiParams, cursor });
    setPages((all) => [...all, page.spans]);
    setCursor(page.next_cursor || null);
  };

  const totalSpans = (series.data?.buckets || []).reduce((t, b) => t + b.spans, 0);
  const maxDuration = Math.max(...rows.map((s) => s.end_time_ns - s.start_time_ns), 1);

  const apply = (next) => setApplied({ ...next });
  const setSort = (key) => {
    const current = applied.sort;
    const next = current === key ? '-' + key : current === '-' + key ? '' : key;
    apply({ ...query, sort: next });
    setQuery((q) => ({ ...q, sort: next }));
  };

  const openRow = (row) => go(['trace', row.trace_id], { span: row.span_id });

  useKeys((event, { typing }) => {
    if (typing || event.metaKey || event.ctrlKey) return;
    if (event.key === 'j') { event.preventDefault(); setSelected((at) => Math.min(rows.length - 1, at + 1)); }
    else if (event.key === 'k') { event.preventDefault(); setSelected((at) => Math.max(0, at - 1)); }
    else if (event.key === 'Enter' && rows[selected]) { event.preventDefault(); openRow(rows[selected]); }
    else if (event.key === 'e') { event.preventDefault(); go(['store'], toHash(applied)); }
    else if (event.key === '/') {
      event.preventDefault();
      // `/` goes to the text search, not the predicate builder: it is the
      // control somebody reaching for a keyboard shortcut means.
      document.querySelector('input[aria-label="Search text"]')?.focus();
    }
  }, [rows, selected, applied, go]);

  return <div style={{ display: 'grid', gap: 12, maxWidth: 1780 }}>
    <datalist id="traza-fields">
      {['service', 'name', 'status', 'session', 'duration_ms',
        'attr.llm.usage.total_tokens', 'attr.llm.cost_usd', 'attr.gen_ai.request.model',
        'attr.session.id', 'attr.http.status_code'].map((f) => <option key={f} value={f} />)}
    </datalist>

    <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
      <span style={{
        fontSize: 11, textTransform: 'uppercase', letterSpacing: 'var(--tracking-caps)',
        color: 'var(--ink-faint)', fontWeight: 500, marginRight: 2,
      }}>views</span>
      {views.map((view) => <Chip key={view.name} onClick={() => { setQuery(view.query); apply(view.query); }}>
        {view.name}
      </Chip>)}
      <Chip dashed onClick={() => {
        // `globalThis`, not `window`: this reads the browser global, and saying
        // so makes it impossible for a later local named `window` to silently
        // turn it into `undefined` again.
        const name = globalThis.prompt?.('Name this view') || `view ${views.length + 1}`;
        setViews([...views, { name, query: applied }]);
      }}>+ save current</Chip>
    </div>

    <Card pad="12px 14px">
      <div style={{ display: 'flex', alignItems: 'baseline', marginBottom: 10 }}>
        <Eyebrow>Query</Eyebrow>
        <span style={{ marginLeft: 10, fontSize: 12, color: 'var(--ink-faint)' }}>
          all predicates are ANDed · <code style={{ fontFamily: 'var(--font-mono)', fontSize: 12 }}>≠</code>{' '}
          keeps spans that never recorded the key
        </span>
        <span style={{ marginLeft: 'auto', display: 'flex', gap: 6 }}>
          <Chip onClick={() => navigator.clipboard?.writeText(toCurl(applied, globalThis.location?.origin || ''))}>
            Copy as curl
          </Chip>
          <Chip tone="primary" onClick={() => apply(query)}>Search</Chip>
        </span>
      </div>
      {/* Content search gets the full width and the first position: it is the
          one filter a user reaches for without already knowing the schema.
          Word matching, not substring — "refund" finds "Refund the order" and
          not "refunds" — which is why it is a field of its own rather than a
          predicate row whose operator would have to lie about the semantics. */}
      <input value={query.content}
        onChange={(e) => setQuery((q) => ({ ...q, content: e.target.value }))}
        onKeyDown={(e) => { if (e.key === 'Enter') apply(query); }}
        aria-label="Search text"
        placeholder="search text in prompts, completions and events (words, not substrings)"
        title={'Finds spans containing every word given, anywhere in their text.\n'
          + 'Word matching, not substring: "refund" finds "Refund the order", not "refunds".\n'
          + 'Multiple words are ANDed, in any order.'}
        style={{
          width: '100%', padding: '5px 9px', marginBottom: 8,
          border: '1px solid var(--hairline)', borderRadius: 'var(--radius-control)',
          background: 'var(--bg)', fontSize: 13, color: 'var(--ink)', outline: 'none',
        }} />
      <div style={{ display: 'grid', gap: 5 }}>
        {query.preds.map((pred, index) => <PredicateRow key={pred.id} pred={pred}
          onChange={(next) => setQuery((q) => ({
            ...q, preds: q.preds.map((p, i) => (i === index ? next : p)),
          }))}
          onRemove={() => {
            const next = { ...query, preds: query.preds.filter((_, i) => i !== index) };
            setQuery(next);
            apply(next);
          }} />)}
        <div style={{ display: 'flex', gap: 10, alignItems: 'center', marginTop: 2 }}>
          <Chip dashed onClick={() => setQuery((q) => ({ ...q, preds: [...q.preds, predicate()] }))}>
            + predicate
          </Chip>
          <span style={{ fontSize: 12, color: 'var(--ink-faint)' }}>
            press <Kbd>/</Kbd> for the text search · a predicate narrows it, for example{' '}
            <code style={{ fontFamily: 'var(--font-mono)', fontSize: 12 }}>min_attr.llm.usage.total_tokens ≥ 4000</code>
          </span>
        </div>
      </div>
    </Card>

    <Card pad="10px 14px 12px">
      <div style={{ display: 'flex', alignItems: 'center', gap: 10, marginBottom: 8 }}>
        <span style={{
          fontSize: 11, textTransform: 'uppercase', letterSpacing: 'var(--tracking-caps)',
          color: 'var(--ink-faint)', fontWeight: 500,
        }}>volume</span>
        <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>
          drag to set the window — <span style={{ fontFamily: 'var(--font-mono)', color: 'var(--ink)' }}>
            {fmtWindowLabel(timeWindow.sinceNs, timeWindow.untilNs)}
          </span> selected
        </span>
        <span style={{ marginLeft: 'auto', display: 'flex', gap: 6 }}>
          {RANGES.map((r) => <Chip key={r.id} mono
            active={applied.range === r.id}
            onClick={() => { const next = { ...query, range: r.id }; setQuery(next); apply(next); }}>{r.label}</Chip>)}
        </span>
      </div>
      <VolumeBrush buckets={series.data?.buckets || []} bucketNs={series.data?.bucket_ns}
        sinceNs={series.data?.since_ns}
        onSelect={(range) => {
          const next = { ...query, range: { sinceNs: range.sinceNs, untilNs: range.untilNs } };
          setQuery(next);
          apply(next);
        }} />
    </Card>

    <div style={{ display: 'flex', alignItems: 'center', gap: 12, padding: '0 2px', flexWrap: 'wrap' }}>
      <QueryCost cost={search.data?.cost} shown={totalSpans || rows.length} />
      <span style={{ marginLeft: 'auto', display: 'flex', gap: 6 }}>
        <Chip onClick={() => go(['store'], toHash(applied))}>Export NDJSON</Chip>
        <Chip onClick={() => go(['datasets'], toHash(applied))}>Make a dataset</Chip>
      </span>
    </div>

    <LoadingBar active={search.loading} />
    {search.error ? <ErrorState what={search.error.what} next={search.error.next} onRetry={search.reload} /> : null}

    {!search.error && rows.length === 0 && !search.loading ? <EmptyState
      message="No spans match this query. Widen the window, or drop a predicate." /> : null}

    {rows.length ? <Card pad="0" style={{ overflow: 'hidden' }}>
      <div role="row" style={{
        display: 'grid', gridTemplateColumns: GRID, background: 'var(--bg-sunken)',
        borderBottom: '1px solid var(--hairline)',
      }}>
        {COLUMNS.map((column) => {
          const active = applied.sort === column.sort || applied.sort === '-' + column.sort;
          return <div key={column.key} role="columnheader"
            onClick={column.sort ? () => setSort(column.sort) : undefined}
            style={{
              padding: '6px 10px', fontSize: 12, fontWeight: 500,
              color: active ? 'var(--ink)' : 'var(--ink-muted)',
              cursor: column.sort ? 'pointer' : 'default', textAlign: column.align || 'left',
              whiteSpace: 'nowrap', userSelect: 'none',
            }}>
            {column.label}
            {column.sort ? <span style={{ color: 'var(--ink-faint)', marginLeft: 4 }}>
              {applied.sort === column.sort ? '↑' : applied.sort === '-' + column.sort ? '↓' : ''}
            </span> : null}
          </div>;
        })}
      </div>
      {rows.map((span, index) => {
        const duration = span.end_time_ns - span.start_time_ns;
        const error = span.status === 'error';
        // `llmUsage` is the shared mirror of src/semconv.rs. This screen used
        // to carry its own third copy of the precedence, which had drifted:
        // it read only the deprecated `gen_ai.usage.{prompt,completion}_tokens`
        // and a `llm.usage.prompt_tokens` key Traza never recognized, so a
        // span using the current OTel `input`/`output` names — the ones the
        // server and the trace detail both resolve — showed a blank cell here.
        const usage = llmUsage(span);
        return <div key={span.trace_id + span.span_id} role="row" tabIndex={0}
          onClick={() => openRow(span)} onFocus={() => setSelected(index)}
          onKeyDown={(e) => { if (e.key === 'Enter') openRow(span); }}
          style={{
            display: 'grid', gridTemplateColumns: GRID, alignItems: 'center',
            borderBottom: '1px solid var(--hairline)', cursor: 'pointer',
            background: index === selected ? 'var(--bg-sunken)' : 'transparent',
            minHeight: 'var(--row-h)',
          }}>
          <Cell mono muted>{fmtClockNs(span.start_time_ns)}</Cell>
          <Cell>{span.service}</Cell>
          <Cell>{span.name}</Cell>
          <Cell mono muted>{span.trace_id.slice(0, 10)}</Cell>
          <div style={{ padding: 'var(--row-py) 10px' }}>
            <div style={{ height: 7, background: 'var(--bg-sunken)', borderRadius: 1.5, overflow: 'hidden' }}>
              <div style={{
                height: '100%', width: Math.max(1, (duration / maxDuration) * 100) + '%',
                background: error ? 'var(--error)' : 'var(--accent)', borderRadius: 1.5,
              }} />
            </div>
          </div>
          <Cell mono align="right">{fmtDurationNs(duration)}</Cell>
          <Cell mono muted align="right">{usage?.totalTokens ? fmtNum(usage.totalTokens) : ''}</Cell>
          <Cell mono align="right" color="var(--accent)">
            {usage?.costUsd != null ? fmtCost(usage.costUsd) : ''}
          </Cell>
          <Cell color={error ? 'var(--error)' : 'var(--ink-muted)'}>{span.status || '—'}</Cell>
        </div>;
      })}
      <div style={{ display: 'flex', alignItems: 'center', gap: 12, padding: '10px 12px' }}>
        {cursor ? <Chip onClick={loadMore}>Load more</Chip> : null}
        <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>
          <span style={{ fontFamily: 'var(--font-mono)', color: 'var(--ink)' }}>{fmtNum(rows.length)}</span>
          {totalSpans ? <> of <span style={{ fontFamily: 'var(--font-mono)', color: 'var(--ink)' }}>{fmtNum(totalSpans)}</span></> : null} shown
        </span>
        <span style={{ marginLeft: 'auto', fontSize: 12, color: 'var(--ink-faint)' }}>
          <Kbd>j</Kbd> <Kbd>k</Kbd> move · <Kbd>↵</Kbd> open trace · <Kbd>e</Kbd> export
        </span>
      </div>
    </Card> : null}
  </div>;
}

function Cell({ children, mono, muted, align, color }) {
  return <div style={{
    padding: 'var(--row-py) 10px',
    fontFamily: mono ? 'var(--font-mono)' : 'inherit',
    fontSize: mono ? 12 : 'var(--cell-fs)',
    fontVariantNumeric: mono ? 'tabular-nums' : undefined,
    color: color || (muted ? 'var(--ink-muted)' : 'var(--ink)'),
    textAlign: align || 'left',
    overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
  }}>{children}</div>;
}

