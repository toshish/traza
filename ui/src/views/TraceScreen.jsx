import React from 'react';
import { api } from '../lib/api.js';
import { useRead, useKeys, navigate } from '../lib/route.js';
import {
  waterfallOrder,
  criticalPath,
  childrenOf,
  selfTimeNs,
  llmUsage,
  sessionIdOf,
  collectPayloadRefs,
} from '../lib/spans.js';
import { fmtCost, fmtDurationNs, fmtNum, fmtPercent, fmtTimeNs } from '../lib/format.js';
import { Card, Chip, Eyebrow, ErrorState, Mono, Skeleton } from '../components/primitives/Chrome.jsx';
import { TimeRuler } from '../components/charts/Marks.jsx';
import { AttrTree } from '../components/data/AttrTree.jsx';
import { Modal } from '../components/primitives/Modal.jsx';
import { Input } from '../components/primitives/Input.jsx';
import { Button } from '../components/primitives/Button.jsx';
import { PayloadBody } from '../components/trace/MessageList.jsx';

// One trace, on a time axis. The old waterfall positioned bars proportionally
// with no ruler, no zoom, no collapse and no self time — readable at nine
// spans, unusable at two hundred. This one is a reading instrument: you can
// see when things happened, why the trace took as long as it did, and which
// part of that is the span's own work.

const LABEL_WIDTH = 300;

function Tile({ label, value, unit, note, tone }) {
  return <div style={{ padding: '10px 14px', borderRight: '1px solid var(--hairline)', minWidth: 0 }}>
    <div style={{ fontSize: 11, color: 'var(--ink-muted)', marginBottom: 3 }}>{label}</div>
    <div style={{
      fontFamily: 'var(--font-mono)', fontSize: 16, fontVariantNumeric: 'tabular-nums',
      color: tone || 'var(--accent)', whiteSpace: 'nowrap',
    }}>
      {value}{unit ? <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>{unit}</span> : null}
      {note ? <span style={{ fontSize: 11, color: 'var(--ok)', marginLeft: 5 }}>{note}</span> : null}
    </div>
  </div>;
}

function AnnotateModal({ open, traceId, spanId, onClose, onRecorded, pushToast }) {
  const [name, setName] = React.useState('');
  const [value, setValue] = React.useState('');
  const [source, setSource] = React.useState('');
  const [comment, setComment] = React.useState('');
  const [busy, setBusy] = React.useState(false);
  const record = async () => {
    if (!name) return;
    setBusy(true);
    try {
      let parsed = value;
      try { parsed = JSON.parse(value); } catch (e) { /* a plain string is a valid value */ }
      await api.annotate({ trace_id: traceId, span_id: spanId || '', name, value: parsed, source, comment });
      pushToast({ status: 'ok', title: 'Annotation recorded', detail: `${name} on ${spanId ? 'span' : 'trace'}` });
      setName(''); setValue(''); setSource(''); setComment('');
      onRecorded();
      onClose();
    } catch (e) {
      pushToast({ status: 'error', title: e.what || 'Annotation failed', detail: e.next });
    } finally { setBusy(false); }
  };
  return <Modal open={open} title="Annotate" onClose={onClose} footer={<>
    <Button onClick={onClose}>Cancel</Button>
    <Button variant="primary" onClick={record} disabled={busy || !name}>Record annotation</Button>
  </>}>
    <div style={{ display: 'grid', gap: 8 }}>
      <label style={{ display: 'grid', gap: 4, color: 'var(--ink-muted)', fontSize: 12 }}>name
        <Input mono value={name} onChange={setName} placeholder="groundedness" /></label>
      <label style={{ display: 'grid', gap: 4, color: 'var(--ink-muted)', fontSize: 12 }}>value — JSON literal or string
        <Input mono value={value} onChange={setValue} placeholder="0.9" /></label>
      <label style={{ display: 'grid', gap: 4, color: 'var(--ink-muted)', fontSize: 12 }}>source
        <Input mono value={source} onChange={setSource} placeholder="human:reviewer" /></label>
      <label style={{ display: 'grid', gap: 4, color: 'var(--ink-muted)', fontSize: 12 }}>comment
        <Input value={comment} onChange={setComment} /></label>
    </div>
  </Modal>;
}

export function TraceScreen({ traceId, go, params, pushToast }) {
  const trace = useRead((signal) => api.trace(traceId, signal), [traceId]);
  const [zoom, setZoom] = React.useState(null);       // {from, to} as fractions
  const [collapsed, setCollapsed] = React.useState(() => new Set());
  const [agentMode, setAgentMode] = React.useState(false);
  const [annotating, setAnnotating] = React.useState(false);
  const [drag, setDrag] = React.useState(null);
  const rulerRef = React.useRef(null);

  const selectedId = params.get('span') || '';
  const selectSpan = (id) => navigate(['trace', traceId], id ? { span: id } : undefined, { replace: true });

  const spans = trace.data?.spans || [];
  const annotations = trace.data?.annotations || [];

  const analysis = React.useMemo(() => {
    if (!spans.length) return null;
    const t0 = Math.min(...spans.map((s) => s.start_time_ns));
    const t1 = Math.max(...spans.map((s) => s.end_time_ns));
    const kids = childrenOf(spans);
    return {
      t0, t1, span: Math.max(1, t1 - t0),
      kids,
      path: criticalPath(spans),
      ordered: waterfallOrder(spans),
      services: new Set(spans.map((s) => s.service)).size,
      errors: spans.filter((s) => s.status === 'error').length,
      tokens: spans.reduce((total, s) => total + (llmUsage(s)?.totalTokens || 0), 0),
      cost: spans.reduce((total, s) => total + (llmUsage(s)?.costUsd || 0), 0),
    };
  }, [spans]);

  // Agent mode hides the plumbing: HTTP and framework spans that carry no
  // model call and no error are noise when you are reading a conversation.
  const visible = React.useMemo(() => {
    if (!analysis) return [];
    let rows = analysis.ordered;
    if (agentMode) {
      rows = rows.filter(({ span }) => llmUsage(span) || span.status === 'error'
        || /agent|tool|llm|chat|completion|task/i.test(span.name));
    }
    // A collapsed span hides its descendants, which is what makes a 200-span
    // trace readable — you open the branch you are interested in.
    const hidden = new Set();
    for (const { span, depth } of rows) {
      if (hidden.has(span.parent_span_id)) { hidden.add(span.span_id); continue; }
      if (collapsed.has(span.span_id)) hidden.add(span.span_id);
    }
    return rows.filter(({ span }) => !hidden.has(span.parent_span_id) || !span.parent_span_id);
  }, [analysis, agentMode, collapsed]);

  const view = React.useMemo(() => {
    if (!analysis) return null;
    const from = zoom ? zoom.from : 0;
    const to = zoom ? zoom.to : 1;
    return {
      startNs: analysis.t0 + analysis.span * from,
      endNs: analysis.t0 + analysis.span * to,
      from, to,
    };
  }, [analysis, zoom]);

  useKeys((event, { typing }) => {
    if (typing || event.metaKey || event.ctrlKey) return;
    if (event.key === 'Escape' && zoom) { event.preventDefault(); setZoom(null); }
    if (event.key === 'a') { event.preventDefault(); setAgentMode((on) => !on); }
    if (event.key === 'j' || event.key === 'k') {
      event.preventDefault();
      const index = visible.findIndex(({ span }) => span.span_id === selectedId);
      const next = event.key === 'j'
        ? Math.min(visible.length - 1, index + 1) : Math.max(0, index - 1);
      if (visible[next]) selectSpan(visible[next].span.span_id);
    }
  }, [zoom, visible, selectedId]);

  const selected = spans.find((s) => s.span_id === selectedId) || null;

  const fractionAt = (clientX) => {
    const box = rulerRef.current?.getBoundingClientRect();
    if (!box?.width) return 0;
    return Math.min(1, Math.max(0, (clientX - box.left) / box.width));
  };

  if (trace.loading && !trace.data) return <Skeleton height={280} />;
  if (trace.error) {
    return <ErrorState
      what={trace.error.status === 404 ? 'Trace not found.' : trace.error.what}
      next={trace.error.status === 404 ? 'The id may be wrong, or its spans expired under TTL.' : trace.error.next}
      onRetry={trace.reload} />;
  }
  if (!analysis || !view) return null;

  const scale = (ns) => ((ns - view.startNs) / Math.max(1, view.endNs - view.startNs)) * 100;

  return <div style={{
    display: 'grid', gridTemplateColumns: 'minmax(560px,1fr) minmax(320px,428px)',
    gap: 14, maxWidth: 1900, alignItems: 'start',
  }}>
    <div style={{ display: 'grid', gap: 12, minWidth: 0 }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
        <Mono size={13}>{traceId}</Mono>
        <Chip onClick={() => navigator.clipboard?.writeText(traceId)}>copy id</Chip>
        <span style={{ marginLeft: 'auto', display: 'flex', gap: 6 }}>
          <Chip active={agentMode} onClick={() => setAgentMode((on) => !on)}>Agent mode</Chip>
          {sessionIdOf(spans[0] || {}) ? <Chip onClick={() => go(['sessions', sessionIdOf(spans[0]).id])}>Session</Chip> : null}
          <Chip onClick={() => go(['conversation', 'traces', traceId])}>Conversation</Chip>
          <Chip onClick={() => go(['compare'], { a: traceId })}>Compare</Chip>
          <Chip tone="primary" onClick={() => setAnnotating(true)}>Annotate</Chip>
        </span>
      </div>

      <Card pad="0" style={{ display: 'grid', gridTemplateColumns: 'repeat(7,1fr)', overflow: 'hidden' }}>
        <Tile label="duration" value={fmtDurationNs(analysis.span)} />
        <Tile label="critical path" value={fmtPercent(
          [...analysis.path].reduce((total, id) => {
            const span = spans.find((s) => s.span_id === id);
            return total + (span ? selfTimeNs(span, analysis.kids.get(id) || []) : 0);
          }, 0), analysis.span).replace('%', '')} unit="%" />
        <Tile label="spans" value={fmtNum(spans.length)} tone="var(--ink)" />
        <Tile label="services" value={fmtNum(analysis.services)} tone="var(--ink)" />
        <Tile label="tokens" value={fmtNum(analysis.tokens)} />
        <Tile label="cost USD" value={fmtCost(analysis.cost)} />
        <Tile label="errors" value={fmtNum(analysis.errors)}
          tone={analysis.errors ? 'var(--error)' : 'var(--ink)'} />
      </Card>

      <Card pad="10px 14px 12px">
        <div style={{ display: 'flex', alignItems: 'center', gap: 10, marginBottom: 8, flexWrap: 'wrap' }}>
          <Eyebrow>waterfall</Eyebrow>
          <Legend color="var(--accent)" label="critical path" />
          <Legend color="var(--measure-2)" label="off path" />
          <Legend color="var(--series-4)" label="time in children" />
          <span style={{ marginLeft: 'auto', fontSize: 12, color: 'var(--ink-faint)' }}>
            drag on the ruler to zoom · click a caret to collapse a subtree
            {zoom ? <> · <span onClick={() => setZoom(null)} style={{ color: 'var(--accent)', cursor: 'pointer' }}>reset</span></> : null}
          </span>
        </div>

        {/* Minimap: the whole trace, always, with the zoom window drawn on it.
            Zooming without one is how you get lost in a long trace. */}
        <div style={{
          position: 'relative', height: 26, background: 'var(--bg-sunken)',
          borderRadius: 3, overflow: 'hidden', marginBottom: 8,
        }}>
          {spans.map((span) => <div key={span.span_id} style={{
            position: 'absolute',
            left: ((span.start_time_ns - analysis.t0) / analysis.span) * 100 + '%',
            width: Math.max(0.4, ((span.end_time_ns - span.start_time_ns) / analysis.span) * 100) + '%',
            top: 3 + (analysis.ordered.find((r) => r.span.span_id === span.span_id)?.depth || 0) * 3,
            height: 2,
            background: span.status === 'error' ? 'var(--error)'
              : analysis.path.has(span.span_id) ? 'var(--accent)' : 'var(--measure-2)',
            borderRadius: 1,
          }} />)}
          {zoom ? <div style={{
            position: 'absolute', left: zoom.from * 100 + '%', right: (1 - zoom.to) * 100 + '%',
            top: 0, bottom: 0, border: '1px solid var(--accent)',
            background: 'rgba(198,93,59,0.08)', pointerEvents: 'none',
          }} /> : null}
        </div>

        <div style={{ display: 'grid', gridTemplateColumns: `${LABEL_WIDTH}px 1fr 78px 74px`, gap: 8, alignItems: 'end' }}>
          <span style={{ fontSize: 11, color: 'var(--ink-faint)' }}>span</span>
          <div ref={rulerRef}
            onMouseDown={(e) => { const at = fractionAt(e.clientX); setDrag({ from: at, to: at }); }}
            onMouseMove={(e) => { if (drag) setDrag((d) => ({ ...d, to: fractionAt(e.clientX) })); }}
            onMouseUp={(e) => {
              if (!drag) return;
              const to = fractionAt(e.clientX);
              const [lo, hi] = drag.from <= to ? [drag.from, to] : [to, drag.from];
              // Below one percent there is nothing to see; treat it as a click.
              if (hi - lo > 0.01) {
                const base = zoom || { from: 0, to: 1 };
                const width = base.to - base.from;
                setZoom({ from: base.from + lo * width, to: base.from + hi * width });
              }
              setDrag(null);
            }}
            onMouseLeave={() => setDrag(null)}
            style={{ position: 'relative', cursor: 'ew-resize', userSelect: 'none' }}>
            <TimeRuler startNs={view.startNs} endNs={view.endNs} />
            {drag && Math.abs(drag.to - drag.from) > 0.005 ? <div style={{
              position: 'absolute', left: Math.min(drag.from, drag.to) * 100 + '%',
              right: (1 - Math.max(drag.from, drag.to)) * 100 + '%', top: 0, bottom: -4,
              background: 'rgba(198,93,59,0.12)', borderLeft: '1px solid var(--accent)',
              borderRight: '1px solid var(--accent)', pointerEvents: 'none',
            }} /> : null}
          </div>
          <span style={{ fontSize: 11, color: 'var(--ink-faint)', textAlign: 'right' }}>total</span>
          <span style={{ fontSize: 11, color: 'var(--ink-faint)', textAlign: 'right' }}>self time</span>
        </div>

        <div style={{ marginTop: 4 }}>
          {visible.map(({ span, depth }) => {
            const kids = analysis.kids.get(span.span_id) || [];
            const duration = span.end_time_ns - span.start_time_ns;
            const self = selfTimeNs(span, kids);
            const onPath = analysis.path.has(span.span_id);
            const error = span.status === 'error';
            const usage = llmUsage(span);
            const left = scale(span.start_time_ns);
            const width = Math.max(0.3, scale(span.end_time_ns) - left);
            // The self-time segment is drawn at the head of the bar: the part
            // of the bar that is this span's own work rather than a child's.
            const selfWidth = duration ? (self / duration) * width : width;
            const isCollapsed = collapsed.has(span.span_id);
            return <div key={span.span_id}
              onClick={() => selectSpan(span.span_id)}
              style={{
                display: 'grid', gridTemplateColumns: `${LABEL_WIDTH}px 1fr 78px 74px`, gap: 8,
                alignItems: 'center', minHeight: 'var(--row-h)', cursor: 'pointer',
                background: span.span_id === selectedId ? 'var(--bg-sunken)' : 'transparent',
                borderRadius: 'var(--radius-control)',
              }}>
              <div style={{
                paddingLeft: 4 + depth * 13, display: 'flex', alignItems: 'center', gap: 5,
                minWidth: 0, fontSize: 'var(--cell-fs)',
              }}>
                <span onClick={(e) => {
                  e.stopPropagation();
                  setCollapsed((set) => {
                    const next = new Set(set);
                    if (next.has(span.span_id)) next.delete(span.span_id); else next.add(span.span_id);
                    return next;
                  });
                }}
                  style={{
                    width: 10, color: 'var(--ink-faint)', cursor: kids.length ? 'pointer' : 'default',
                    fontSize: 9, flex: 'none',
                  }}>{kids.length ? (isCollapsed ? '▸' : '▾') : ''}</span>
                <span style={{
                  overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
                  color: error ? 'var(--error)' : 'var(--ink)',
                }}>{span.name}</span>
                <span style={{ color: 'var(--ink-faint)', fontSize: 11, whiteSpace: 'nowrap' }}>· {span.service}</span>
                {isCollapsed && kids.length
                  ? <span style={{ color: 'var(--ink-faint)', fontSize: 11 }}>+{kids.length}</span> : null}
              </div>
              <div style={{ position: 'relative', height: 9 }}>
                <div style={{
                  position: 'absolute', left: left + '%', width: width + '%', top: 1, height: 7,
                  background: error ? 'var(--error)' : onPath ? 'var(--accent)' : 'var(--measure-2)',
                  borderRadius: 'var(--radius-bar)', opacity: error || onPath ? 1 : 0.85,
                }} />
                {kids.length ? <div style={{
                  position: 'absolute', left: left + '%', width: selfWidth + '%', top: 1, height: 7,
                  background: error ? 'var(--error)' : onPath ? 'var(--accent-hover)' : 'var(--measure-4)',
                  borderRadius: 'var(--radius-bar)',
                }} /> : null}
                {/* The model label trails its bar, but only while there is
                    room: past ~68% it would run under the duration columns,
                    and an overlapping label is worse than an absent one. */}
                {usage?.model && left + width < 68 ? <span style={{
                  position: 'absolute', left: `calc(${left + width}% + 6px)`, top: -3,
                  fontFamily: 'var(--font-mono)', fontSize: 11, color: 'var(--ink-faint)',
                  whiteSpace: 'nowrap', pointerEvents: 'none',
                }}>{usage.model}{usage.totalTokens ? ` · ${fmtNum(usage.totalTokens)}` : ''}</span> : null}
              </div>
              <div style={{
                fontFamily: 'var(--font-mono)', fontSize: 12, fontVariantNumeric: 'tabular-nums',
                textAlign: 'right', color: 'var(--ink)',
              }}>{fmtDurationNs(duration)}</div>
              <div style={{
                fontFamily: 'var(--font-mono)', fontSize: 12, fontVariantNumeric: 'tabular-nums',
                textAlign: 'right', color: 'var(--ink-muted)',
              }}>{fmtDurationNs(self)}</div>
            </div>;
          })}
        </div>
      </Card>
    </div>

    <SpanPanel span={selected} annotations={annotations} traceId={traceId}
      onAnnotate={() => setAnnotating(true)} go={go} pushToast={pushToast} />

    <AnnotateModal open={annotating} traceId={traceId} spanId={selectedId}
      onClose={() => setAnnotating(false)} onRecorded={trace.reload} pushToast={pushToast} />
  </div>;
}

function Legend({ color, label }) {
  return <span style={{ display: 'inline-flex', alignItems: 'center', gap: 5, fontSize: 11, color: 'var(--ink-muted)' }}>
    <span style={{ width: 10, height: 4, background: color, borderRadius: 1 }} />{label}
  </span>;
}

function SpanPanel({ span, annotations, traceId, onAnnotate, go, pushToast }) {
  const [payload, setPayload] = React.useState(null);
  if (!span) {
    return <Card>
      <div style={{ fontSize: 13, color: 'var(--ink-muted)' }}>
        Select a span in the waterfall for its detail.
      </div>
      <div style={{ marginTop: 12 }}><Chip onClick={onAnnotate}>Annotate trace</Chip></div>
    </Card>;
  }
  const usage = llmUsage(span);
  const error = span.status === 'error';
  const refs = collectPayloadRefs(span);
  const mine = annotations.filter((a) => a.span_id === span.span_id);
  return <Card pad="12px 14px" style={{ position: 'sticky', top: 60 }}>
    <div style={{ display: 'flex', alignItems: 'center', gap: 8, marginBottom: 10, flexWrap: 'wrap' }}>
      <span style={{ fontSize: 14, fontWeight: 600, color: 'var(--ink)', minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis' }}>
        {span.name}
      </span>
      <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>{span.service}</span>
      {error ? <span style={{
        marginLeft: 'auto', fontSize: 11, fontWeight: 500, padding: '1px 7px',
        borderRadius: 'var(--radius-control)', background: 'var(--error-tint)', color: 'var(--error)',
      }}>error</span> : null}
    </div>

    <div style={{ display: 'grid', gridTemplateColumns: 'repeat(2,1fr)', gap: 8, marginBottom: 12 }}>
      {[
        ['duration', fmtDurationNs(span.end_time_ns - span.start_time_ns)],
        ['start', fmtTimeNs(span.start_time_ns).slice(11)],
        ...(usage?.model ? [['model', usage.model]] : []),
        ...(usage?.totalTokens ? [['tokens', fmtNum(usage.totalTokens)]] : []),
        ...(usage?.costUsd != null ? [['cost USD', fmtCost(usage.costUsd)]] : []),
        ['span id', span.span_id],
      ].map(([label, value]) => <div key={label} style={{ minWidth: 0 }}>
        <div style={{ fontSize: 11, color: 'var(--ink-muted)' }}>{label}</div>
        <div style={{
          fontFamily: 'var(--font-mono)', fontSize: 12, color: 'var(--ink)',
          overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
        }}>{value}</div>
      </div>)}
    </div>

    {error ? <div style={{
      background: 'var(--error-tint)', border: '1px solid var(--error)',
      borderRadius: 'var(--radius-control)', padding: '9px 11px', marginBottom: 12,
      fontSize: 12, lineHeight: '18px', color: 'var(--ink)',
    }}>
      <div style={{ fontWeight: 500 }}>This span failed.</div>
      <div style={{ color: 'var(--ink-muted)', marginTop: 2 }}>
        Group every span with this signature to see whether it is one incident or a pattern.
      </div>
      <div style={{ marginTop: 8, display: 'flex', gap: 6 }}>
        <Chip onClick={() => go(['failures'])}>See all failures</Chip>
        <Chip onClick={() => go(['compare'], { a: traceId })}>Compare with a clean run</Chip>
      </div>
    </div> : null}

    {Object.keys(span.attributes || {}).length ? <div style={{ marginBottom: 12 }}>
      <Eyebrow style={{ marginBottom: 6 }}>Attributes</Eyebrow>
      <AttrTree data={span.attributes} />
    </div> : null}

    {(span.events || []).length ? <div style={{ marginBottom: 12 }}>
      <Eyebrow style={{ marginBottom: 6 }}>Events</Eyebrow>
      {span.events.map((event, i) => <div key={i} style={{ marginBottom: 6 }}>
        <div style={{ fontSize: 12, color: 'var(--ink)' }}>
          <code>{event.name}</code>{' '}
          <span style={{ color: 'var(--ink-faint)', fontFamily: 'var(--font-mono)' }}>
            {fmtTimeNs(event.timestamp_ns).slice(11)}
          </span>
        </div>
        {Object.keys(event.attributes || {}).length
          ? <AttrTree data={event.attributes} style={{ marginTop: 2 }} /> : null}
      </div>)}
    </div> : null}

    {refs.length ? <div style={{ marginBottom: 12 }}>
      <Eyebrow style={{ marginBottom: 6 }}>Offloaded payloads</Eyebrow>
      {refs.map((ref, i) => <div key={i} style={{ display: 'flex', alignItems: 'center', gap: 8, fontSize: 12, marginBottom: 4 }}>
        <span style={{ color: 'var(--ink-muted)' }}>{ref.key}</span>
        <span style={{ fontFamily: 'var(--font-mono)', color: 'var(--accent)' }}>{fmtNum(ref.bytes)} B</span>
        <Chip onClick={async () => {
          try { setPayload({ key: ref.key, text: await api.payload(ref.ref) }); }
          catch (e) { pushToast({ status: 'error', title: e.what || 'Payload fetch failed', detail: e.next }); }
        }}>Load</Chip>
      </div>)}
    </div> : null}

    {mine.length ? <div style={{ marginBottom: 12 }}>
      <Eyebrow style={{ marginBottom: 6 }}>Scores</Eyebrow>
      <div style={{ display: 'flex', gap: 6, flexWrap: 'wrap' }}>
        {mine.map((a, i) => <Chip key={i} mono>
          {a.name} {typeof a.value === 'number' ? a.value : JSON.stringify(a.value)}
        </Chip>)}
      </div>
    </div> : null}

    <Chip onClick={onAnnotate}>Annotate this span</Chip>

    <Modal open={!!payload} title={payload?.key || ''} width={720} onClose={() => setPayload(null)}>
      {payload ? <PayloadBody text={payload.text} hintKey={payload.key}
        onLoadPayload={(ref) => api.payload(ref)} /> : null}
    </Modal>
  </Card>;
}
