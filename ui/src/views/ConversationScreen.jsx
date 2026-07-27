import React from 'react';
import { api } from '../lib/api.js';
import { useRead } from '../lib/route.js';
import { llmMessages, llmUsage } from '../lib/spans.js';
import { fmtCost, fmtDurationNs, fmtNum, fmtTimeNs } from '../lib/format.js';
import { Card, Chip, Eyebrow, ErrorState, EmptyState, LoadingBar, Mono } from '../components/primitives/Chrome.jsx';
import { MessageList } from '../components/trace/MessageList.jsx';

// What was said, rather than what ran. A waterfall answers the second; this
// answers the first — and adds the turn rail the review asked for, so cost and
// latency per turn are readable without opening each span.
//
// Deduplication matters: consecutive turns re-send the whole history, so
// replaying every span's prompt verbatim shows the same user message a dozen
// times. Each turn contributes only what is new since the previous turn.

function turnsFromSpans(spans) {
  const ordered = [...spans].sort((a, b) => a.start_time_ns - b.start_time_ns);
  const turns = [];
  const seen = new Set();
  for (const span of ordered) {
    const messages = llmMessages(span);
    if (!messages.length) continue;
    const fresh = [];
    for (const message of messages) {
      // Identity of a message = role + its rendered parts. A prompt already
      // shown by an earlier turn is history, not a new thing said.
      const key = JSON.stringify([message.role || '', message.parts]);
      if (message.direction === 'prompt' && seen.has(key)) continue;
      seen.add(key);
      fresh.push(message);
    }
    if (fresh.length) turns.push({ span, messages: fresh, usage: llmUsage(span) });
  }
  return turns;
}

export function ConversationScreen({ kind, id, go }) {
  const isSession = kind === 'sessions';
  const read = useRead(
    (signal) => (isSession
      ? api.spans({ session: id, limit: 1000 }, signal).then((page) => ({ spans: page.spans }))
      : api.trace(id, signal)),
    [kind, id],
  );
  const [focused, setFocused] = React.useState(0);
  const turnRefs = React.useRef([]);

  const turns = React.useMemo(() => turnsFromSpans(read.data?.spans || []), [read.data]);
  const totals = turns.reduce((acc, turn) => ({
    tokens: acc.tokens + (turn.usage?.totalTokens || 0),
    cost: acc.cost + (turn.usage?.costUsd || 0),
    duration: acc.duration + (turn.span.end_time_ns - turn.span.start_time_ns),
  }), { tokens: 0, cost: 0, duration: 0 });

  const jump = (index) => {
    setFocused(index);
    turnRefs.current[index]?.scrollIntoView({ behavior: 'smooth', block: 'start' });
  };

  if (read.error) {
    return <ErrorState
      what={read.error.status === 404 ? 'Not found.' : read.error.what}
      next={read.error.next} onRetry={read.reload} />;
  }

  return <div style={{ display: 'grid', gridTemplateColumns: '236px minmax(0,1fr)', gap: 16, maxWidth: 1400 }}>
    {/* The turn rail: every turn with what it cost and how long it took, so
        the expensive turn is findable without reading the whole exchange. */}
    <div style={{ position: 'sticky', top: 60, alignSelf: 'start' }}>
      <Eyebrow style={{ marginBottom: 8 }}>Turns</Eyebrow>
      <div style={{ display: 'grid', gap: 2 }}>
        {turns.map((turn, index) => {
          const duration = turn.span.end_time_ns - turn.span.start_time_ns;
          const error = turn.span.status === 'error';
          return <div key={index} onClick={() => jump(index)} role="link" tabIndex={0}
            onKeyDown={(e) => { if (e.key === 'Enter') jump(index); }}
            style={{
              display: 'grid', gridTemplateColumns: '22px 1fr auto', gap: 6, alignItems: 'baseline',
              padding: '4px 6px', borderRadius: 'var(--radius-control)', cursor: 'pointer',
              borderLeft: `2px solid ${index === focused ? 'var(--accent)' : 'transparent'}`,
              background: index === focused ? 'var(--bg-sunken)' : 'transparent',
            }}>
            <span style={{
              fontFamily: 'var(--font-mono)', fontSize: 11, fontVariantNumeric: 'tabular-nums',
              color: 'var(--ink-faint)',
            }}>{index + 1}</span>
            <span style={{
              fontSize: 12, color: error ? 'var(--error)' : 'var(--ink)',
              overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
            }}>{turn.usage?.model || turn.span.name}</span>
            <span style={{
              fontFamily: 'var(--font-mono)', fontSize: 11, fontVariantNumeric: 'tabular-nums',
              color: 'var(--accent)',
            }}>{turn.usage?.costUsd ? fmtCost(turn.usage.costUsd) : fmtDurationNs(duration)}</span>
          </div>;
        })}
      </div>
      {turns.length ? <div style={{
        marginTop: 10, paddingTop: 9, borderTop: '1px solid var(--hairline)',
        fontSize: 11, color: 'var(--ink-muted)', display: 'grid', gap: 3,
      }}>
        <div>{fmtNum(turns.length)} turns</div>
        <div><Mono color="var(--accent)">{fmtNum(totals.tokens)}</Mono> tokens</div>
        <div><Mono color="var(--accent)">{fmtCost(totals.cost)}</Mono> USD</div>
        <div><Mono>{fmtDurationNs(totals.duration)}</Mono> in model calls</div>
      </div> : null}
    </div>

    <div style={{ minWidth: 0, display: 'grid', gap: 12 }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
        <Mono size={13}>{id}</Mono>
        <span style={{ marginLeft: 'auto', display: 'flex', gap: 6 }}>
          <Chip onClick={() => go(isSession ? ['sessions', id] : ['trace', id])}>
            Back to {isSession ? 'session' : 'trace'}
          </Chip>
        </span>
      </div>

      <LoadingBar active={read.loading} />
      {read.data && !turns.length ? <EmptyState
        message={<>No messages here. Traza reads prompts and completions from{' '}
          <code>gen_ai.prompt.*</code>, <code>gen_ai.completion.*</code>, OpenLLMetry's{' '}
          <code>llm.prompts.*</code>, and offloaded payload references.</>} /> : null}

      {turns.map((turn, index) => {
        const duration = turn.span.end_time_ns - turn.span.start_time_ns;
        return <Card key={index} pad="12px 14px"
          style={{ scrollMarginTop: 70 }}>
          <div ref={(node) => { turnRefs.current[index] = node; }} />
          <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap', marginBottom: 8 }}>
            <span style={{
              fontFamily: 'var(--font-mono)', fontSize: 11, fontVariantNumeric: 'tabular-nums',
              color: 'var(--ink-faint)',
            }}>{fmtTimeNs(turn.span.start_time_ns).slice(11)}</span>
            <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>{turn.span.name}</span>
            {turn.usage?.model ? <Chip mono>{turn.usage.model}</Chip> : null}
            {turn.usage?.totalTokens != null ? <Mono color="var(--ink-faint)">
              {fmtNum(turn.usage.totalTokens)} tok
            </Mono> : null}
            {turn.usage?.costUsd ? <Mono color="var(--accent)">${fmtCost(turn.usage.costUsd)}</Mono> : null}
            <Mono color="var(--ink-faint)">{fmtDurationNs(duration)}</Mono>
            {turn.span.status === 'error' ? <span style={{
              fontSize: 11, fontWeight: 500, padding: '1px 7px', borderRadius: 'var(--radius-control)',
              background: 'var(--error-tint)', color: 'var(--error)',
            }}>error</span> : null}
            <span style={{ marginLeft: 'auto' }}>
              <Chip onClick={() => go(['trace', turn.span.trace_id], { span: turn.span.span_id })}>
                Jump to span
              </Chip>
            </span>
          </div>
          <MessageList messages={turn.messages} onLoadPayload={(ref) => api.payload(ref)} />
        </Card>;
      })}
    </div>
  </div>;
}
