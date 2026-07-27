import React from 'react';
import { api } from '../lib/api.js';
import { useRead } from '../lib/route.js';
import { RANGES, windowOf } from '../lib/query.js';
import { fmtAgo, fmtCost, fmtDurationNs, fmtNum, fmtWindow } from '../lib/format.js';
import { Card, Chip, ErrorState, EmptyState, LoadingBar, Mono } from '../components/primitives/Chrome.jsx';

// Sessions gained the time window the API always had, a sort, an efficiency
// column, and errors that click through. The list and Analytics used to
// disagree about "now" because only one of them had a window at all.

const SORTS = [
  { id: 'recent', label: 'most recent', of: (s) => -s.last_end_ns },
  { id: 'cost', label: 'costliest', of: (s) => -s.cost_usd },
  { id: 'errors', label: 'most errors', of: (s) => -s.error_count },
  { id: 'turns', label: 'longest', of: (s) => -s.trace_count },
  { id: 'efficiency', label: 'worst cost/turn', of: (s) => -(s.trace_count ? s.cost_usd / s.trace_count : 0) },
];

export function SessionsScreen({ go }) {
  const [range, setRange] = React.useState('24h');
  const [sort, setSort] = React.useState('recent');
  const [limit, setLimit] = React.useState(100);
  const window = React.useMemo(() => windowOf(range), [range]);

  const sessions = useRead((signal) => api.sessions({
    since: window.sinceNs ? Math.round(window.sinceNs) : undefined,
    until: window.untilNs ? Math.round(window.untilNs) : undefined,
    limit,
  }, signal), [window.sinceNs, window.untilNs, limit]);

  const rows = React.useMemo(() => {
    const list = [...(sessions.data?.sessions || [])];
    const by = SORTS.find((s) => s.id === sort);
    return list.sort((a, b) => by.of(a) - by.of(b));
  }, [sessions.data, sort]);

  return <div style={{ display: 'grid', gap: 12, maxWidth: 1560 }}>
    <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
      {RANGES.map((r) => <Chip key={r.id} mono active={range === r.id} onClick={() => setRange(r.id)}>{r.label}</Chip>)}
      <span style={{ marginLeft: 12, fontSize: 11, textTransform: 'uppercase', letterSpacing: 'var(--tracking-caps)', color: 'var(--ink-faint)', fontWeight: 500 }}>sort</span>
      {SORTS.map((s) => <Chip key={s.id} active={sort === s.id} onClick={() => setSort(s.id)}>{s.label}</Chip>)}
    </div>

    <LoadingBar active={sessions.loading} />
    {sessions.error ? <ErrorState what={sessions.error.what} next={sessions.error.next} onRetry={sessions.reload} /> : null}
    {sessions.data && !rows.length ? <EmptyState
      message={<span>No sessions in this window. A session is any set of spans sharing a recognized
        session key — <code>session.id</code>, <code>gen_ai.conversation.id</code>, or a
        <code>traceloop.association.properties.*</code> key:</span>}
      command={'"attributes": {"gen_ai.conversation.id": "chat-4711", "gen_ai.request.model": "…"}'} /> : null}

    {rows.length ? <Card pad="0" style={{ overflow: 'hidden' }}>
      <div style={{
        display: 'grid', gridTemplateColumns: '1fr 76px 74px 84px 92px 92px 100px 72px 64px',
        background: 'var(--bg-sunken)', borderBottom: '1px solid var(--hairline)',
      }}>
        {['session', 'turns', 'spans', 'LLM calls', 'tokens', 'cost USD', 'cost / turn', 'errors', 'last']
          .map((label, i) => <div key={label} style={{
            padding: '6px 10px', fontSize: 12, fontWeight: 500, color: 'var(--ink-muted)',
            textAlign: i === 0 ? 'left' : 'right', whiteSpace: 'nowrap',
          }}>{label}</div>)}
      </div>
      {rows.map((session) => <div key={session.session_id}
        onClick={() => go(['sessions', session.session_id])} role="link" tabIndex={0}
        onKeyDown={(e) => { if (e.key === 'Enter') go(['sessions', session.session_id]); }}
        style={{
          display: 'grid', gridTemplateColumns: '1fr 76px 74px 84px 92px 92px 100px 72px 64px',
          alignItems: 'center', borderBottom: '1px solid var(--hairline)',
          cursor: 'pointer', minHeight: 'var(--row-h)',
        }}>
        <div style={{ padding: 'var(--row-py) 10px', minWidth: 0 }}>
          <div style={{
            fontFamily: 'var(--font-mono)', fontSize: 12, color: 'var(--ink)',
            overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
          }}>{session.session_id}</div>
          <div style={{ fontSize: 11, color: 'var(--ink-faint)' }}>
            {fmtWindow(session.first_start_ns, session.last_end_ns)}
          </div>
        </div>
        <Num>{fmtNum(session.trace_count)}</Num>
        <Num muted>{fmtNum(session.span_count)}</Num>
        <Num muted>{fmtNum(session.llm_calls)}</Num>
        <Num>{fmtNum(session.total_tokens)}</Num>
        <Num accent>{fmtCost(session.cost_usd)}</Num>
        <Num muted>{session.trace_count ? (session.cost_usd / session.trace_count).toFixed(5) : '—'}</Num>
        <Num tone={session.error_count ? 'var(--error)' : 'var(--ink-faint)'}>{session.error_count}</Num>
        <Num muted>{fmtAgo(session.last_end_ns)}</Num>
      </div>)}
      {rows.length >= limit ? <div style={{ padding: '10px 12px' }}>
        <Chip onClick={() => setLimit((n) => n + 100)}>Load more</Chip>
      </div> : null}
    </Card> : null}
  </div>;
}

export function SessionScreen({ sessionId, go }) {
  const detail = useRead((signal) => api.session(sessionId, signal), [sessionId]);
  const data = detail.data;

  if (detail.error) {
    return <ErrorState
      what={detail.error.status === 404 ? 'Session not found.' : detail.error.what}
      next={detail.error.status === 404 ? 'The id may be wrong, or its spans expired under TTL.' : detail.error.next}
      onRetry={detail.reload} />;
  }
  if (!data) return <LoadingBar active />;

  const traces = data.traces || [];
  const maxCost = Math.max(...traces.map((t) => t.cost_usd || 0), 0);

  return <div style={{ display: 'grid', gap: 12, maxWidth: 1560 }}>
    <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
      <Mono size={13}>{sessionId}</Mono>
      <span style={{ fontSize: 12, color: 'var(--ink-faint)' }}>
        {fmtWindow(data.first_start_ns, data.last_end_ns)}
        {data.session_attribute && data.session_attribute !== 'session.id' ? ` · via ${data.session_attribute}` : ''}
      </span>
      <span style={{ marginLeft: 'auto', display: 'flex', gap: 6 }}>
        <Chip onClick={() => navigator.clipboard?.writeText(sessionId)}>copy id</Chip>
        <Chip onClick={() => go(['conversation', 'sessions', sessionId])}>Conversation</Chip>
        <Chip onClick={() => go(['traces'], { q: `session|=|${sessionId}` })}>Search its spans</Chip>
      </span>
    </div>

    <div style={{ display: 'grid', gridTemplateColumns: 'repeat(7,1fr)', gap: 12 }}>
      {[
        ['turns', fmtNum(data.trace_count)],
        ['spans', fmtNum(data.span_count)],
        ['LLM calls', fmtNum(data.llm_calls)],
        ['tokens', fmtNum(data.total_tokens)],
        ['cost USD', fmtCost(data.cost_usd)],
        ['cost / turn', data.trace_count ? (data.cost_usd / data.trace_count).toFixed(5) : '—'],
        ['errors', fmtNum(data.error_count)],
      ].map(([label, value]) => <Card key={label} pad="12px 14px">
        <div style={{ fontSize: 12, color: 'var(--ink-muted)', marginBottom: 5 }}>{label}</div>
        <div style={{
          fontFamily: 'var(--font-mono)', fontSize: 18, lineHeight: '26px', fontWeight: 500,
          fontVariantNumeric: 'tabular-nums',
          color: label === 'errors' && data.error_count ? 'var(--error)' : 'var(--accent)',
        }}>{value}</div>
      </Card>)}
    </div>

    <Card pad="0" style={{ overflow: 'hidden' }}>
      <div style={{
        display: 'grid', gridTemplateColumns: '1fr 132px 86px 76px 94px 92px 76px',
        background: 'var(--bg-sunken)', borderBottom: '1px solid var(--hairline)',
      }}>
        {['turn', 'cost', 'window', 'spans', 'tokens', 'cost USD', 'errors'].map((label, i) => (
          <div key={label} style={{
            padding: '6px 10px', fontSize: 12, fontWeight: 500, color: 'var(--ink-muted)',
            textAlign: i === 0 || i === 1 ? 'left' : 'right', whiteSpace: 'nowrap',
          }}>{label}</div>
        ))}
      </div>
      {traces.map((trace) => <div key={trace.trace_id}
        onClick={() => go(['trace', trace.trace_id])} role="link" tabIndex={0}
        onKeyDown={(e) => { if (e.key === 'Enter') go(['trace', trace.trace_id]); }}
        style={{
          display: 'grid', gridTemplateColumns: '1fr 132px 86px 76px 94px 92px 76px',
          alignItems: 'center', borderBottom: '1px solid var(--hairline)',
          cursor: 'pointer', minHeight: 'var(--row-h)',
        }}>
        <div style={{ padding: 'var(--row-py) 10px', minWidth: 0 }}>
          <div style={{
            fontSize: 'var(--cell-fs)', color: 'var(--ink)',
            overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
          }}>{trace.root_name}</div>
          <div style={{ fontFamily: 'var(--font-mono)', fontSize: 11, color: 'var(--ink-faint)' }}>
            {trace.trace_id.slice(0, 18)}
          </div>
        </div>
        <div style={{ padding: 'var(--row-py) 10px' }}>
          <div style={{ height: 7, background: 'var(--bg-sunken)', borderRadius: 1.5, overflow: 'hidden' }}>
            <div style={{
              height: '100%', width: maxCost ? Math.max(1, (trace.cost_usd / maxCost) * 100) + '%' : 0,
              background: trace.error_count ? 'var(--error)' : 'var(--accent)', borderRadius: 1.5,
            }} />
          </div>
        </div>
        <Num muted>{fmtDurationNs(trace.last_end_ns - trace.first_start_ns)}</Num>
        <Num muted>{fmtNum(trace.span_count)}</Num>
        <Num>{fmtNum(trace.total_tokens)}</Num>
        <Num accent>{fmtCost(trace.cost_usd)}</Num>
        <Num tone={trace.error_count ? 'var(--error)' : 'var(--ink-faint)'}>{trace.error_count}</Num>
      </div>)}
    </Card>
  </div>;
}

function Num({ children, muted, accent, tone }) {
  return <div style={{
    padding: 'var(--row-py) 10px', fontFamily: 'var(--font-mono)', fontSize: 12,
    fontVariantNumeric: 'tabular-nums', textAlign: 'right', whiteSpace: 'nowrap',
    color: tone || (accent ? 'var(--accent)' : muted ? 'var(--ink-muted)' : 'var(--ink)'),
  }}>{children}</div>;
}
