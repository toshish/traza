import React from 'react';
import { api } from '../lib/api.js';
import { fmtDurationNs, fmtTimeNs, fmtNum, fmtCost } from '../lib/format.js';
import { waterfallOrder, collectPayloadRefs, llmUsage, sessionIdOf, llmMessages } from '../lib/spans.js';
import { Section } from '../components/Section.jsx';
import { Button } from '../components/primitives/Button.jsx';
import { Input } from '../components/primitives/Input.jsx';
import { Modal } from '../components/primitives/Modal.jsx';
import { Tag } from '../components/primitives/Tag.jsx';
import { DataTable } from '../components/data/DataTable.jsx';
import { KeyValuePanel } from '../components/data/KeyValuePanel.jsx';
import { AttrTree } from '../components/data/AttrTree.jsx';
import { CodeBlock } from '../components/data/CodeBlock.jsx';
import { CopyButton } from '../components/data/CopyButton.jsx';
import { TraceWaterfall } from '../components/trace/TraceWaterfall.jsx';
import { ScoreChip } from '../components/trace/ScoreChip.jsx';
import { AnnotationTimeline } from '../components/trace/AnnotationTimeline.jsx';
import { MessageList } from '../components/trace/MessageList.jsx';
import { ErrorState } from '../components/feedback/ErrorState.jsx';
import { LoadingBar } from '../components/feedback/LoadingBar.jsx';

/** Parse an annotation value the way the API stores it: JSON literal when it
    is one ("0.9", "true"), plain string otherwise. */
function parseValue(text) {
  try { return JSON.parse(text); } catch (e) { return text; }
}

function AnnotateModal({ open, traceId, spanId, onClose, onRecorded, pushToast }) {
  const [name, setName] = React.useState('');
  const [value, setValue] = React.useState('');
  const [source, setSource] = React.useState('');
  const [comment, setComment] = React.useState('');
  const [wholeTrace, setWholeTrace] = React.useState(!spanId);
  const [busy, setBusy] = React.useState(false);
  React.useEffect(() => { setWholeTrace(!spanId); }, [spanId, open]);
  const record = async () => {
    if (!name) return;
    setBusy(true);
    try {
      await api.annotate({
        trace_id: traceId,
        span_id: wholeTrace ? '' : (spanId || ''),
        name,
        value: parseValue(value),
        source,
        comment,
      });
      pushToast({ status: 'ok', title: 'Annotation recorded', detail: name + ' on ' + (wholeTrace ? 'trace' : 'span') });
      setName(''); setValue(''); setSource(''); setComment('');
      onRecorded();
      onClose();
    } catch (e) {
      pushToast({ status: 'error', title: e.what || 'Annotation failed', detail: e.next });
    } finally {
      setBusy(false);
    }
  };
  return <Modal open={open} title="Annotate" onClose={onClose} footer={<>
    <Button onClick={onClose}>Cancel</Button>
    <Button variant="primary" onClick={record} disabled={busy || !name}>Record annotation</Button>
  </>}>
    <div style={{ display: 'grid', gap: 8 }}>
      <label style={{ display: 'grid', gap: 4, color: 'var(--ink-muted)', fontSize: 'var(--text-12)' }}>name (for example groundedness, thumbs)
        <Input mono value={name} onChange={setName} placeholder="quality" /></label>
      <label style={{ display: 'grid', gap: 4, color: 'var(--ink-muted)', fontSize: 'var(--text-12)' }}>value — JSON literal or string
        <Input mono value={value} onChange={setValue} placeholder="0.9" /></label>
      <label style={{ display: 'grid', gap: 4, color: 'var(--ink-muted)', fontSize: 'var(--text-12)' }}>source (convention: human:&lt;who&gt; or eval:&lt;evaluator&gt;)
        <Input mono value={source} onChange={setSource} placeholder="human:reviewer" /></label>
      <label style={{ display: 'grid', gap: 4, color: 'var(--ink-muted)', fontSize: 'var(--text-12)' }}>comment
        <Input value={comment} onChange={setComment} placeholder="" /></label>
      {spanId ? <label style={{ display: 'flex', gap: 6, alignItems: 'center', fontSize: 'var(--text-13)', color: 'var(--ink)' }}>
        <input type="checkbox" checked={wholeTrace} onChange={(e) => setWholeTrace(e.target.checked)} />
        annotate the whole trace instead of span <code>{spanId}</code>
      </label> : null}
    </div>
  </Modal>;
}

function LlmMessages({ span }) {
  const messages = llmMessages(span);
  const loadPayload = React.useCallback((ref) => api.payload(ref), []);
  if (!messages.length) return null;
  return <div style={{ marginTop: 12 }}>
    <div style={{ fontSize: 'var(--text-12)', fontWeight: 500, color: 'var(--ink-muted)', textTransform: 'uppercase', letterSpacing: 'var(--tracking-caps)', marginBottom: 6 }}>Messages</div>
    <MessageList messages={messages} onLoadPayload={loadPayload} />
  </div>;
}

function PayloadPanel({ span, pushToast }) {
  const refs = collectPayloadRefs(span);
  const [openRef, setOpenRef] = React.useState(null);
  const [content, setContent] = React.useState('');
  const [busy, setBusy] = React.useState(false);
  if (!refs.length) return null;
  const load = async (r) => {
    setBusy(true);
    try {
      setContent(await api.payload(r.ref));
      setOpenRef(r);
    } catch (e) {
      pushToast({ status: 'error', title: e.what || 'Payload fetch failed', detail: e.next });
    } finally {
      setBusy(false);
    }
  };
  return <div style={{ marginTop: 12 }}>
    <div style={{ fontSize: 'var(--text-12)', fontWeight: 500, color: 'var(--ink-muted)', textTransform: 'uppercase', letterSpacing: 'var(--tracking-caps)', marginBottom: 6 }}>Offloaded payloads</div>
    <div style={{ display: 'grid', gap: 6 }}>
      {refs.map((r, i) => <div key={i} style={{ display: 'flex', alignItems: 'center', gap: 8, fontSize: 'var(--text-12)', minWidth: 0 }}>
        <span style={{ color: 'var(--ink-muted)', whiteSpace: 'nowrap' }}>{r.where} · {r.key}</span>
        <code style={{ color: 'var(--ink-faint)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{r.ref}</code>
        <span style={{ fontFamily: 'var(--font-mono)', color: 'var(--accent)', whiteSpace: 'nowrap' }}>{fmtNum(r.bytes)} B</span>
        <Button size="sm" onClick={() => load(r)} disabled={busy}>Load payload</Button>
      </div>)}
    </div>
    <Modal open={!!openRef} title={openRef ? openRef.key : ''} width={720} onClose={() => setOpenRef(null)}>
      {openRef ? <>
        <div style={{ display: 'flex', gap: 8, alignItems: 'center', marginBottom: 8, color: 'var(--ink-muted)', fontSize: 'var(--text-12)' }}>
          <code>{openRef.ref}</code><CopyButton text={content} label="copy contents" />
        </div>
        <CodeBlock code={content} copyable={false} style={{ maxHeight: '55vh', whiteSpace: 'pre-wrap' }} />
      </> : null}
    </Modal>
  </div>;
}

function SpanDetail({ span, annotations, onAnnotate, openSession, pushToast }) {
  const usage = llmUsage(span);
  const session = sessionIdOf(span);
  const spanAnnotations = annotations.filter((a) => a.span_id === span.span_id);
  return <div style={{ minWidth: 0 }}>
    <KeyValuePanel title="Span detail" items={[
      { key: 'span_id', value: span.span_id },
      ...(span.parent_span_id ? [{ key: 'parent', value: span.parent_span_id }] : []),
      { key: 'service', value: span.service },
      { key: 'status', value: span.status || '—', color: span.status === 'error' ? 'var(--error)' : undefined },
      { key: 'start', value: fmtTimeNs(span.start_time_ns) },
      { key: 'duration', value: fmtDurationNs(span.end_time_ns - span.start_time_ns), measured: true },
      ...(session ? [{ key: session.key, value: session.id }] : []),
    ]} />
    {session ? <div style={{ marginTop: 6 }}><Button variant="ghost" size="sm" onClick={() => openSession(session.id)}>View session</Button></div> : null}
    {usage ? <div style={{ display: 'flex', gap: 6, marginTop: 12, flexWrap: 'wrap' }}>
      {usage.provider ? <Tag mono>{String(usage.provider)}</Tag> : null}
      {usage.model ? <Tag mono>{String(usage.model)}</Tag> : null}
      {usage.totalTokens != null ? <ScoreChip name="tokens" value={fmtNum(usage.totalTokens)} /> : null}
      {usage.promptTokens != null ? <ScoreChip name="prompt" value={fmtNum(usage.promptTokens)} /> : null}
      {usage.completionTokens != null ? <ScoreChip name="completion" value={fmtNum(usage.completionTokens)} /> : null}
      {usage.costUsd != null ? <ScoreChip name="cost USD" value={fmtCost(usage.costUsd)} /> : null}
      {usage.stopReason ? <Tag>{String(usage.stopReason)}</Tag> : null}
    </div> : null}
    <LlmMessages span={span} />
    {Object.keys(span.attributes || {}).length ? <div style={{ marginTop: 12 }}>
      <div style={{ fontSize: 'var(--text-12)', fontWeight: 500, color: 'var(--ink-muted)', textTransform: 'uppercase', letterSpacing: 'var(--tracking-caps)', marginBottom: 6 }}>Attributes</div>
      <AttrTree data={span.attributes} />
    </div> : null}
    {(span.events || []).length ? <div style={{ marginTop: 12 }}>
      <div style={{ fontSize: 'var(--text-12)', fontWeight: 500, color: 'var(--ink-muted)', textTransform: 'uppercase', letterSpacing: 'var(--tracking-caps)', marginBottom: 6 }}>Events</div>
      {span.events.map((event, i) => <div key={i} style={{ marginBottom: 6 }}>
        <div style={{ fontSize: 'var(--text-12)', color: 'var(--ink)' }}>
          <code>{event.name}</code> <span style={{ color: 'var(--ink-faint)', fontFamily: 'var(--font-mono)', fontSize: 'var(--text-12)' }}>{fmtTimeNs(event.timestamp_ns)}</span>
        </div>
        {Object.keys(event.attributes || {}).length ? <AttrTree data={event.attributes} style={{ marginTop: 2 }} /> : null}
      </div>)}
    </div> : null}
    {(span.links || []).length ? <div style={{ marginTop: 12 }}>
      <div style={{ fontSize: 'var(--text-12)', fontWeight: 500, color: 'var(--ink-muted)', textTransform: 'uppercase', letterSpacing: 'var(--tracking-caps)', marginBottom: 6 }}>Links</div>
      {span.links.map((link, i) => <div key={i} style={{ fontSize: 'var(--text-12)', fontFamily: 'var(--font-mono)', color: 'var(--ink-muted)', wordBreak: 'break-all' }}>
        <a href={'#/traces/' + encodeURIComponent(link.trace_id)}>{link.trace_id}</a> · {link.span_id}
      </div>)}
    </div> : null}
    <PayloadPanel span={span} pushToast={pushToast} />
    {spanAnnotations.length ? <div style={{ display: 'flex', gap: 6, marginTop: 12, flexWrap: 'wrap' }}>
      {spanAnnotations.map((a, i) => <ScoreChip key={i} name={a.name}
        value={typeof a.value === 'number' ? String(a.value) : JSON.stringify(a.value)} source={a.source || undefined} />)}
    </div> : null}
    <div style={{ marginTop: 12 }}>
      <Button size="sm" onClick={onAnnotate}>Annotate</Button>
    </div>
  </div>;
}

/** One trace: waterfall, span detail, annotation timeline. */
export function TraceView({ traceId, selectedSpanId, selectSpan, openSession, openConversation, onBack, pushToast }) {
  const [data, setData] = React.useState(null);
  const [error, setError] = React.useState(null);
  const [loading, setLoading] = React.useState(true);
  const [annotating, setAnnotating] = React.useState(false);

  const fetchTrace = React.useCallback(async () => {
    setLoading(true); setError(null);
    try {
      setData(await api.trace(traceId));
    } catch (e) {
      setError(e); setData(null);
    } finally {
      setLoading(false);
    }
  }, [traceId]);
  React.useEffect(() => { fetchTrace(); }, [fetchTrace]);

  const spans = data ? data.spans : [];
  const annotations = data ? data.annotations || [] : [];
  const ordered = React.useMemo(() => waterfallOrder(spans), [spans]);
  const selected = spans.find((s) => s.span_id === selectedSpanId) || null;
  const traceAnnotations = annotations.filter((a) => !a.span_id);

  return <Section title={'Trace ' + traceId}
    action={<div style={{ display: 'flex', gap: 6 }}>
      <CopyButton text={traceId} label="copy id" />
      {openConversation ? <Button variant="ghost" size="sm" onClick={openConversation}>Conversation</Button> : null}
      {onBack ? <Button variant="ghost" size="sm" onClick={onBack}>Back</Button> : null}
      <Button variant="ghost" size="sm" onClick={fetchTrace}>Refresh</Button>
    </div>}>
    <LoadingBar active={loading} style={{ marginBottom: 8 }} />
    {error ? <ErrorState what={error.status === 404 ? 'Trace not found.' : error.what}
      next={error.status === 404 ? 'The id may be wrong, or its spans expired under TTL.' : error.next} /> : null}
    {data ? <div style={{ display: 'grid', gridTemplateColumns: 'minmax(0, 1.5fr) minmax(280px, 1fr)', gap: 20 }}>
      <div style={{ minWidth: 0, overflowX: 'auto' }}>
        <TraceWaterfall labelWidth={220}
          spans={ordered.map(({ span, depth }) => ({
            id: span.span_id, name: span.name, service: span.service,
            startNs: span.start_time_ns, endNs: span.end_time_ns,
            depth, error: span.status === 'error',
          }))}
          selectedId={selectedSpanId}
          onSelect={(s) => selectSpan(s.id)} />
        {annotations.length ? <div style={{ marginTop: 16 }}>
          <div style={{ fontSize: 'var(--text-12)', fontWeight: 500, color: 'var(--ink-muted)', textTransform: 'uppercase', letterSpacing: 'var(--tracking-caps)', marginBottom: 8 }}>Annotations</div>
          <AnnotationTimeline items={[...annotations]
            .sort((a, b) => b.timestamp_ns - a.timestamp_ns)
            .map((a) => ({
              time: fmtTimeNs(a.timestamp_ns).slice(11),
              name: a.name,
              value: typeof a.value === 'number' ? String(a.value) : JSON.stringify(a.value),
              source: (a.source || '') + (a.span_id ? ' · span ' + a.span_id : ' · trace'),
              note: a.comment || undefined,
            }))} />
        </div> : null}
      </div>
      {selected
        ? <SpanDetail span={selected} annotations={annotations} onAnnotate={() => setAnnotating(true)}
            openSession={openSession} pushToast={pushToast} />
        : <div style={{ fontSize: 'var(--text-13)', color: 'var(--ink-muted)' }}>
            Select a span in the waterfall for its detail.
            {traceAnnotations.length ? <div style={{ display: 'flex', gap: 6, marginTop: 12, flexWrap: 'wrap' }}>
              {traceAnnotations.map((a, i) => <ScoreChip key={i} name={a.name}
                value={typeof a.value === 'number' ? String(a.value) : JSON.stringify(a.value)} source={a.source || undefined} />)}
            </div> : null}
            <div style={{ marginTop: 12 }}><Button size="sm" onClick={() => setAnnotating(true)}>Annotate trace</Button></div>
          </div>}
    </div> : null}
    <AnnotateModal open={annotating} traceId={traceId} spanId={selected ? selected.span_id : ''}
      onClose={() => setAnnotating(false)} onRecorded={fetchTrace} pushToast={pushToast} />
  </Section>;
}
