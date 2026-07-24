import React from 'react';
import { api } from '../lib/api.js';
import { fmtNum, fmtCost, fmtWindow, fmtDurationNs } from '../lib/format.js';
import { Section } from '../components/Section.jsx';
import { Button } from '../components/primitives/Button.jsx';
import { LoadMore } from '../components/primitives/LoadMore.jsx';
import { DataTable } from '../components/data/DataTable.jsx';
import { StatTile } from '../components/data/StatTile.jsx';
import { CopyButton } from '../components/data/CopyButton.jsx';
import { SessionCard } from '../components/trace/SessionCard.jsx';
import { EmptyState } from '../components/feedback/EmptyState.jsx';
import { ErrorState } from '../components/feedback/ErrorState.jsx';
import { LoadingBar } from '../components/feedback/LoadingBar.jsx';

const PAGE = 100;

/** Sessions list: one card per session, most recent activity first. */
export function SessionsView({ openSession }) {
  const [sessions, setSessions] = React.useState(null);
  const [error, setError] = React.useState(null);
  const [loading, setLoading] = React.useState(true);
  const [limit, setLimit] = React.useState(PAGE);

  const fetchSessions = React.useCallback(async (effectiveLimit) => {
    setLoading(true); setError(null);
    try {
      const data = await api.sessions({ limit: effectiveLimit });
      setSessions(data.sessions || []);
    } catch (e) {
      setError(e); setSessions(null);
    } finally {
      setLoading(false);
    }
  }, []);
  React.useEffect(() => { fetchSessions(limit); }, [fetchSessions]);

  return <Section title="Sessions" action={<Button variant="ghost" size="sm" onClick={() => fetchSessions(limit)}>Refresh</Button>}>
    <LoadingBar active={loading} style={{ marginBottom: 8 }} />
    {error ? <ErrorState what={error.what} next={error.next} /> : null}
    {sessions && !sessions.length ? <EmptyState
      message={<span>No sessions yet. A session is any set of spans sharing a recognized session key — <code>session.id</code>, <code>gen_ai.conversation.id</code>, or a <code>traceloop.association.properties.*</code> key:</span>}
      command={'"attributes": {"gen_ai.conversation.id": "chat-4711", "gen_ai.request.model": "…"}'} /> : null}
    {sessions && sessions.length ? <>
      <div style={{ display: 'grid', gap: 8 }}>
        {sessions.map((s) => <SessionCard key={s.session_id}
          sessionId={s.session_id}
          traces={s.trace_count} spans={s.span_count} llmCalls={s.llm_calls}
          tokens={s.total_tokens} costUsd={s.cost_usd} errors={s.error_count}
          window={fmtWindow(s.first_start_ns, s.last_end_ns)}
          onClick={() => openSession(s.session_id)} />)}
      </div>
      {sessions.length >= limit
        ? <LoadMore shown={sessions.length} loading={loading}
            onClick={() => { const next = limit + PAGE; setLimit(next); fetchSessions(next); }} />
        : null}
    </> : null}
  </Section>;
}

/** One session: aggregate tiles plus the per-trace breakdown. */
export function SessionDetailView({ sessionId, openTrace, openConversation, onBack, filterSpans }) {
  const [detail, setDetail] = React.useState(null);
  const [error, setError] = React.useState(null);
  const [loading, setLoading] = React.useState(true);

  const fetchDetail = React.useCallback(async () => {
    setLoading(true); setError(null);
    try {
      setDetail(await api.session(sessionId));
    } catch (e) {
      setError(e); setDetail(null);
    } finally {
      setLoading(false);
    }
  }, [sessionId]);
  React.useEffect(() => { fetchDetail(); }, [fetchDetail]);

  return <Section title={'Session ' + sessionId}
    action={<div style={{ display: 'flex', gap: 6 }}>
      <CopyButton text={sessionId} label="copy id" />
      {openConversation ? <Button variant="primary" size="sm" onClick={openConversation}>Conversation</Button> : null}
      <Button variant="ghost" size="sm" onClick={() => filterSpans(sessionId)}>Search its spans</Button>
      {onBack ? <Button variant="ghost" size="sm" onClick={onBack}>Back</Button> : null}
      <Button variant="ghost" size="sm" onClick={fetchDetail}>Refresh</Button>
    </div>}>
    <LoadingBar active={loading} style={{ marginBottom: 8 }} />
    {error ? <ErrorState what={error.status === 404 ? 'Session not found.' : error.what}
      next={error.status === 404 ? 'The id may be wrong, or its spans expired under TTL.' : error.next} /> : null}
    {detail ? <>
      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(140px, 1fr))', gap: 12, marginBottom: 16 }}>
        <StatTile label="traces" value={fmtNum(detail.trace_count)} />
        <StatTile label="spans" value={fmtNum(detail.span_count)} />
        <StatTile label="LLM calls" value={fmtNum(detail.llm_calls)} />
        <StatTile label="total tokens" value={fmtNum(detail.total_tokens)} />
        <StatTile label="cost" value={fmtCost(detail.cost_usd)} unit="USD" />
        <StatTile label="errors" value={fmtNum(detail.error_count)} />
      </div>
      <div style={{ fontSize: 'var(--text-12)', color: 'var(--ink-faint)', fontFamily: 'var(--font-mono)', marginBottom: 8 }}>
        {fmtWindow(detail.first_start_ns, detail.last_end_ns)} · prompt {fmtNum(detail.prompt_tokens)} · completion {fmtNum(detail.completion_tokens)}{detail.session_attribute && detail.session_attribute !== 'session.id' ? ' · via ' + detail.session_attribute : ''}
      </div>
      <div style={{ overflowX: 'auto' }}>
        <DataTable density="dense" onRowClick={(r) => openTrace(r.trace_id)} columns={[
          { key: 'root_name', label: 'root span' },
          { key: 'trace_id', label: 'trace', mono: true, maxWidth: 160 },
          { key: 'span_count', label: 'spans', align: 'right', mono: true },
          { key: 'total_tokens', label: 'tokens', align: 'right', mono: true, render: (v) => fmtNum(v) },
          { key: 'cost_usd', label: 'cost USD', align: 'right', mono: true, render: (v) => fmtCost(v) },
          { key: 'dur', label: 'window', align: 'right', mono: true, render: (_, r) => fmtDurationNs(r.last_end_ns - r.first_start_ns) },
          { key: 'error_count', label: 'errors', align: 'right', mono: true, render: (v) => <span style={{ color: v ? 'var(--error)' : 'var(--ink-faint)' }}>{v}</span> },
        ]} rows={detail.traces || []} />
      </div>
    </> : null}
  </Section>;
}
