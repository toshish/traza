import React from 'react';
import { api } from '../lib/api.js';
import { publicBase } from '../lib/base.js';
import { navigate, useRead } from '../lib/route.js';
import { bytesLabel, isPayloadRef, normalizePart } from '../lib/spans.js';
import { fmtDurationNs, fmtNum, fmtTimeNs } from '../lib/format.js';
import {
  fmtMoney, replayEnv, replayPython, routeTenant, singleTenantScope, splitFingerprint, tapeTotals, tenantParams,
} from '../lib/runs.js';
import { Card, Chip, EmptyState, ErrorState, LoadingBar, Mono, PanelHead, Skeleton } from '../components/primitives/Chrome.jsx';
import { Tag } from '../components/primitives/Tag.jsx';
import { Input } from '../components/primitives/Input.jsx';
import { CodeBlock } from '../components/data/CodeBlock.jsx';
import { MessageList } from '../components/trace/MessageList.jsx';
import { UnscopedNote } from '../components/runs/UnscopedNote.jsx';
import { TopologyCard } from '../components/runs/Topology.jsx';
import { NudgesCard } from '../components/runs/Nudges.jsx';
import { ForkModal, ForksCard } from '../components/runs/Fork.jsx';
import { RunForesight } from '../components/insight/RunForesight.jsx';
import { forkable } from '../lib/fork.js';
import { VERDICT_TAG, canWrite } from './GuardScreen.jsx';

// A run read as a tape: its model calls, tool calls and agent invocations in
// order, with what went into each and what came out — and from here, a replay
// of it, a fork from any step, a diff against another run, its agents and
// how they handed control around, and the nudges a person sent it. A run is
// a session id, or `trace:<id>` for a trace with none; sessions and traces
// link here with exactly that rule.

/** Steps the screen asks for at a time. The server holds up to 20,000;
    drawing that many rows would stall the page, and the tape says when it
    was cut. */
const MAX_STEPS = 2000;

/** The most steps the server returns for one tape. */
const TAPE_MAX_STEPS = 20000;

/** Steps to ask for so that `focus` — a step a diff linked to — is on the
    tape: the first page, or as many whole pages as reach it. The diff aligns
    up to 5,000 steps a side, so its links can point past the first page. */
export function stepsToLoad(focus) {
  if (focus == null || focus < MAX_STEPS) return MAX_STEPS;
  return Math.min(TAPE_MAX_STEPS, (Math.floor(focus / MAX_STEPS) + 1) * MAX_STEPS);
}

// Kinds are categories, not measurements, so they stay ink: the accent is
// for the duration bars and the cost column.
const KIND_TAG = { llm: 'neutral', tool: 'neutral', agent: 'neutral' };

export function RunScreen({ runId, go, params, identity, pushToast }) {
  // `undefined` when the route names no tenant: the run's tenant is unknown.
  const tenant = routeTenant(params);
  const scope = tenantParams(tenant);
  const operator = Boolean(identity && identity.operator);
  // Unscoped, an unbound credential reads every tenant's run of this id; the
  // verdict, which is always one tenant's, would describe something else.
  const oneTenant = singleTenantScope(tenant, identity);
  const resolve = params.get('resolve') === 'true';
  const focusStep = params.get('step');
  const focus = focusStep != null && /^[0-9]+$/.test(focusStep) ? Number(focusStep) : null;
  const maxSteps = stepsToLoad(focus);

  // Each tape is kept with the scope it was read in. While a new scope loads
  // the old tape stays on screen, and the replay panel must describe the
  // recording beside it, not the one the route has moved on to.
  const tape = useRead((signal) => api.tape(runId, {
    ...scope, resolve: resolve ? 'true' : undefined, max_steps: maxSteps,
  }, signal).then((body) => ({ body, runId, tenant })), [runId, tenant, resolve, maxSteps]);
  // The guard's verdict is context, not the point: a store started with
  // --no-fuses answers 409 here, and the tape stands on its own.
  const verdict = useRead((signal) => api.fuseRun(runId, scope, signal).catch(() => null), [runId, tenant],
    { skip: !oneTenant });
  // The step a fork is being made from, and a count of forks made here, so
  // the listing reads again after one.
  const [forking, setForking] = React.useState(null);
  const [forked, setForked] = React.useState(0);

  const setParam = (key, value) => {
    const next = new URLSearchParams(params);
    if (value) next.set(key, value); else next.delete(key);
    navigate(['run', runId], Object.fromEntries(next), { replace: true });
  };
  const scopeTo = (name) => {
    const next = new URLSearchParams(params);
    next.set('tenant', name);
    navigate(['run', runId], Object.fromEntries(next), { replace: true });
  };

  if (tape.loading && !tape.data) return <Skeleton height={260} />;
  if (tape.error) {
    return <ErrorState
      what={tape.error.status === 404 ? 'No spans carry this run id.' : tape.error.what}
      next={tape.error.status === 404
        ? 'A run is a session id, or trace:<trace_id> for a trace without one. Its spans may also have expired under TTL, or belong to another tenant.'
        : tape.error.next}
      onRetry={tape.reload} />;
  }
  if (!tape.data) return null;
  const shown = tape.data;
  const data = shown.body;
  const totals = tapeTotals(data);
  const sessionLike = !runId.startsWith('trace:');
  const missing = focus != null && !data.steps.some((step) => step.index === focus);

  return <div style={{ display: 'grid', gap: 14, maxWidth: 1700 }}>
    <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
      <Mono size={13}>{runId}</Mono>
      {data.tenant ? <Mono color="var(--ink-muted)">tenant {data.tenant}</Mono>
        : data.tenant === '' && operator ? <Mono color="var(--ink-muted)">default tenant</Mono> : null}
      <Chip onClick={() => navigator.clipboard?.writeText(runId)}>copy id</Chip>
      {oneTenant && verdict.data && verdict.data.verdict ? <Tag status={VERDICT_TAG[verdict.data.verdict] || 'neutral'} mono>
        guard: {verdict.data.verdict}
      </Tag> : null}
      <span style={{ marginLeft: 'auto', display: 'flex', gap: 6, flexWrap: 'wrap' }}>
        {sessionLike
          ? <Chip onClick={() => go(['sessions', runId], scope)}>Session</Chip>
          : <Chip onClick={() => go(['trace', runId.slice('trace:'.length)])}>Trace</Chip>}
        <Chip onClick={() => go(['guard'])}>Guard</Chip>
        <Chip onClick={() => go(['xray', runId], scope)} title="Every model call's context window, and why the prompt cache broke">X-Ray</Chip>
        <Chip active={resolve} onClick={() => setParam('resolve', resolve ? '' : 'true')}
          title="Inline offloaded payloads, up to the server's --tape-max-bytes">
          {resolve ? 'Payloads inlined' : 'Inline payloads'}
        </Chip>
      </span>
    </div>

    <Card pad="0" className="tz-reflow" style={{ display: 'grid', gridTemplateColumns: 'repeat(6, minmax(0, 1fr))', overflow: 'hidden' }}>
      <Tile label="steps" value={fmtNum(totals.steps)} note={data.truncated ? 'cut' : ''} />
      <Tile label="duration" value={totals.steps ? fmtDurationNs(totals.durationNs) : '—'} />
      <Tile label="tokens" value={fmtNum(totals.tokens)} />
      <Tile label="cost USD" value={totals.priced ? fmtMoney(totals.cost) : '—'}
        title={totals.priced < totals.steps ? `${totals.priced} of ${totals.steps} steps reported a cost` : undefined} />
      <Tile label="errors" value={fmtNum(totals.errors)} tone={totals.errors ? 'var(--error)' : 'var(--ink)'} />
      <Tile label="spans read" value={fmtNum(data.spans_examined)} tone="var(--ink)" />
    </Card>

    {!oneTenant && operator ? <UnscopedNote onScope={scopeTo}>
      This tape is read across every tenant: no tenant is named, so if two tenants use this run id
      their steps are interleaved here, and no guard verdict or foresight is shown, since each is
      one tenant's. Name the tenant to read it in:
    </UnscopedNote> : null}

    {data.truncated ? <div style={{ fontSize: 12, color: 'var(--warn)' }}>
      This tape is cut: the run has more steps than shown (at most {fmtNum(maxSteps)} here), or more
      than 20,000 spans.
    </div> : null}
    {missing ? <div data-testid="missing-step" role="status" style={{ fontSize: 12, color: 'var(--warn)' }}>
      Step {fmtNum(focus)} is not on this tape: it has {fmtNum(data.steps.length)} steps
      {data.truncated ? ', and was cut before the step' : ''}.
    </div> : null}
    {data.resolved_partial ? <div style={{ fontSize: 12, color: 'var(--warn)' }}>
      Some payloads stayed references: the server's per-tape budget (<code>--tape-max-bytes</code>) ran
      out, or their bytes are gone. Each can still be loaded on its own below.
    </div> : null}

    <div className="tz-split" style={{ display: 'grid', gridTemplateColumns: 'minmax(0, 1fr) minmax(320px, 420px)', gap: 14, alignItems: 'start' }}>
      <Tape steps={data.steps} totals={totals} go={go} focusStep={focusStep} onFork={setForking}
        onXray={(index) => go(['xray', runId], { ...(scope || {}), step: String(index) })} />
      <div style={{ display: 'grid', gap: 14, minWidth: 0 }}>
        <RunForesight runId={runId} tenant={tenant} oneTenant={oneTenant} identityKnown={identity != null} go={go} />
        {oneTenant ? <TopologyCard key={'topology:' + replayScopeKey(runId, tenant)} run={runId} tenant={tenant} go={go} /> : null}
        {oneTenant ? <NudgesCard key={'nudges:' + replayScopeKey(runId, tenant)} run={runId} tenant={tenant}
          writer={canWrite(identity)} /> : null}
        <ReplayPanel key={replayScopeKey(shown.runId, shown.tenant)} runId={shown.runId} tenant={shown.tenant}
          tape={data} identity={identity} pushToast={pushToast} />
        <ForksCard key={'forks:' + replayScopeKey(runId, tenant)} run={runId} tenant={tenant} go={go} nonce={forked} />
        <ComparePanel runId={runId} tenant={tenant} go={go} />
      </div>
    </div>

    <ForkModal run={shown.runId} tenant={shown.tenant} step={forking} identity={identity}
      onClose={() => setForking(null)} onCreated={() => setForked((n) => n + 1)} />
  </div>;
}

function Tile({ label, value, note, tone, title }) {
  return <div title={title} style={{ padding: '10px 14px', borderRight: '1px solid var(--hairline)', borderBottom: '1px solid var(--hairline)', minWidth: 0, marginBottom: -1 }}>
    <div style={{ fontSize: 11, color: 'var(--ink-muted)', marginBottom: 3 }}>{label}</div>
    <div style={{
      fontFamily: 'var(--font-mono)', fontSize: 16, fontVariantNumeric: 'tabular-nums',
      color: tone || 'var(--accent)', whiteSpace: 'nowrap',
    }}>{value}{note ? <span style={{ fontSize: 11, color: 'var(--warn)', marginLeft: 6 }}>{note}</span> : null}</div>
  </div>;
}

function stepName(step) {
  return step.model || step.tool || step.agent || step.name;
}

function Tape({ steps, totals, go, focusStep, onFork, onXray }) {
  const [open, setOpen] = React.useState(() => new Set(focusStep != null ? [Number(focusStep)] : []));
  const focusRef = React.useRef(null);
  React.useEffect(() => {
    if (focusStep != null && focusRef.current && focusRef.current.scrollIntoView) {
      focusRef.current.scrollIntoView({ block: 'center' });
    }
  }, [focusStep]);
  if (!steps.length) {
    return <EmptyState message={<span>This run has spans but no steps. A step is a model call, a tool
      call or an agent invocation; workflow wrappers, HTTP and retrieval spans are context around
      steps, so they are not on the tape.</span>} />;
  }
  const span = Math.max(1, totals.durationNs);
  const columns = '36px 58px minmax(160px, 1.4fr) minmax(90px, 1fr) 78px 104px 74px 52px';
  const toggle = (index) => setOpen((set) => {
    const next = new Set(set);
    if (next.has(index)) next.delete(index); else next.add(index);
    return next;
  });
  return <Card pad="0" style={{ overflow: 'hidden' }}>
    <div style={{ padding: '12px 14px 0' }}>
      <PanelHead title="Tape" note="steps in start order · click a step for what went in and what came out" />
    </div>
    <div className="tz-scroll-x">
      <div style={{ minWidth: 720 }} role="table" aria-label="Run steps">
        <div role="row" style={{
          display: 'grid', gridTemplateColumns: columns, background: 'var(--bg-sunken)',
          borderTop: '1px solid var(--hairline)', borderBottom: '1px solid var(--hairline)',
        }}>
          {['#', 'kind', 'step', 'when', 'duration', 'tokens in / out', 'cost', 'status'].map((label, i) => <div key={label} role="columnheader" style={{
            padding: '6px 10px', fontSize: 12, fontWeight: 500, color: 'var(--ink-muted)',
            textAlign: i >= 4 ? 'right' : 'left', whiteSpace: 'nowrap',
          }}>{label}</div>)}
        </div>
        {steps.map((step) => {
          const expanded = open.has(step.index);
          const left = ((step.start_ns - totals.startNs) / span) * 100;
          const width = Math.max(0.5, ((step.duration_ns || 0) / span) * 100);
          const focused = focusStep != null && Number(focusStep) === step.index;
          return <div key={step.index} ref={focused ? focusRef : undefined} data-testid="tape-step">
            <div role="row" onClick={() => toggle(step.index)} tabIndex={0} aria-expanded={expanded}
              onKeyDown={(e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); toggle(step.index); } }}
              style={{
                display: 'grid', gridTemplateColumns: columns, alignItems: 'center', cursor: 'pointer',
                borderBottom: '1px solid var(--hairline)', minHeight: 'var(--row-h)', fontSize: 'var(--cell-fs)',
                background: expanded || focused ? 'var(--bg-sunken)' : 'transparent',
                borderLeft: `2px solid ${focused ? 'var(--accent)' : 'transparent'}`,
              }}>
              <div role="cell" style={{ padding: 'var(--row-py) 10px' }}><Mono color="var(--ink-faint)">{step.index}</Mono></div>
              <div role="cell" style={{ padding: 'var(--row-py) 10px' }}><Tag status={KIND_TAG[step.kind] || 'neutral'} mono>{step.kind}</Tag></div>
              <div role="cell" style={{ padding: 'var(--row-py) 10px', minWidth: 0 }}>
                <div style={{ fontFamily: 'var(--font-mono)', fontSize: 12, color: step.status === 'error' ? 'var(--error)' : 'var(--ink)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                  {stepName(step)}
                </div>
                <div style={{ fontSize: 11, color: 'var(--ink-faint)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
                  {step.name}{step.service ? ' · ' + step.service : ''}
                </div>
              </div>
              <div role="cell" style={{ padding: 'var(--row-py) 10px' }}>
                <div style={{ position: 'relative', height: 7, background: 'var(--bg-sunken)', borderRadius: 1.5 }}>
                  <div style={{
                    position: 'absolute', left: left + '%', width: width + '%', top: 0, bottom: 0,
                    background: step.status === 'error' ? 'var(--error)' : step.kind === 'llm' ? 'var(--accent)' : 'var(--measure-2)',
                    borderRadius: 1.5,
                  }} />
                </div>
              </div>
              <Num>{fmtDurationNs(step.duration_ns || 0)}</Num>
              <Num muted>{step.tokens?.input == null && step.tokens?.output == null ? '—'
                : `${step.tokens.input == null ? '—' : fmtNum(step.tokens.input)} / ${step.tokens.output == null ? '—' : fmtNum(step.tokens.output)}`}</Num>
              <Num accent={typeof step.cost_usd === 'number'}>{typeof step.cost_usd === 'number' ? fmtMoney(step.cost_usd) : '—'}</Num>
              <div role="cell" style={{ padding: 'var(--row-py) 10px', textAlign: 'right' }}>
                <span style={{ fontSize: 11, color: step.status === 'error' ? 'var(--error)' : 'var(--ink-faint)' }}>{step.status}</span>
              </div>
            </div>
            {expanded ? <StepDetail step={step} go={go} onFork={onFork} onXray={onXray} /> : null}
          </div>;
        })}
      </div>
    </div>
  </Card>;
}

function Num({ children, muted, accent }) {
  return <div role="cell" style={{
    padding: 'var(--row-py) 10px', fontFamily: 'var(--font-mono)', fontSize: 12,
    fontVariantNumeric: 'tabular-nums', textAlign: 'right', whiteSpace: 'nowrap',
    color: accent ? 'var(--accent)' : muted ? 'var(--ink-muted)' : 'var(--ink)',
  }}>{children}</div>;
}

function StepDetail({ step, go, onFork, onXray }) {
  const fp = splitFingerprint(step.fingerprint);
  return <div style={{ padding: '10px 14px 14px', borderBottom: '1px solid var(--hairline)', background: 'var(--bg)', display: 'grid', gap: 10 }}>
    <div style={{ display: 'flex', gap: 14, flexWrap: 'wrap', fontSize: 12, color: 'var(--ink-muted)' }}>
      <span>fingerprint <Mono color="var(--ink)">{fp.kind}:{fp.name}</Mono>{fp.hash ? <Mono color="var(--ink-faint)">#{fp.hash}</Mono> : null}</span>
      <span>started <Mono>{fmtTimeNs(step.start_ns)}</Mono></span>
      {step.tokens?.cache_read != null ? <span>cache read <Mono>{fmtNum(step.tokens.cache_read)}</Mono></span> : null}
      {step.tokens?.cache_write != null ? <span>cache write <Mono>{fmtNum(step.tokens.cache_write)}</Mono></span> : null}
      <span style={{ marginLeft: 'auto', display: 'inline-flex', gap: 10, alignItems: 'center', flexWrap: 'wrap' }}>
        {step.kind === 'llm' && onXray ? <a href="#" data-testid="xray-link"
          onClick={(e) => { e.preventDefault(); onXray(step.index); }}>X-Ray this call</a> : null}
        <a href={'#/trace/' + encodeURIComponent(step.trace_id) + '?span=' + encodeURIComponent(step.span_id)}
          onClick={(e) => { e.preventDefault(); go(['trace', step.trace_id], { span: step.span_id }); }}>open the span in its trace</a>
        {onFork && forkable(step) ? <Chip onClick={() => onFork(step)}
          title={`Serve a different ${step.kind === 'llm' ? 'answer' : 'result'} here, or replay up to here and go live`}>Fork from here</Chip> : null}
      </span>
    </div>
    <div className="tz-reflow-1" style={{ display: 'grid', gridTemplateColumns: 'repeat(2, minmax(0, 1fr))', gap: 12 }}>
      <Side title="request" digest={step.request_digest} value={step.request} kind={step.kind} direction="prompt" />
      <Side title="response" digest={step.response_digest} value={step.response} kind={step.kind} direction="completion" />
    </div>
  </div>;
}

function Side({ title, digest, value, kind, direction }) {
  return <div style={{ minWidth: 0 }}>
    <div style={{ display: 'flex', alignItems: 'baseline', gap: 8, marginBottom: 6 }}>
      <span style={{ fontSize: 11, textTransform: 'uppercase', letterSpacing: 'var(--tracking-caps)', color: 'var(--ink-muted)', fontWeight: 500 }}>{title}</span>
      {digest ? <span title={'sha256 ' + digest}><Mono size={11} color="var(--ink-faint)">sha256 {digest.slice(0, 12)}…</Mono></span> : null}
    </div>
    <TapeValue value={value} kind={kind} direction={direction} />
  </div>;
}

/** One tape message as the transcript renderer's shape. The tape folds every
    convention into `{role, content | parts, tool_calls?, finish_reason?}`
    (src/semconv.rs); tool calls sit beside the content, not inside it. */
export function tapeMessage(message, direction) {
  if (message == null || typeof message !== 'object') {
    return { direction, role: undefined, parts: [normalizePart(message)] };
  }
  const parts = [];
  if (Array.isArray(message.parts)) parts.push(...message.parts.map(normalizePart));
  else if (Array.isArray(message.content)) parts.push(...message.content.map(normalizePart));
  else if (message.content != null) parts.push(normalizePart({ content: message.content }));
  for (const call of message.tool_calls || []) {
    parts.push({ kind: 'tool_call', id: call.id, name: call.name || '', args: call.arguments ?? {} });
  }
  if (!parts.length) parts.push({ kind: 'text', text: '' });
  return { direction, role: message.role, finishReason: message.finish_reason, parts };
}

function isMessageList(value) {
  return Array.isArray(value) && value.length > 0
    && value.every((m) => m && typeof m === 'object' && !Array.isArray(m) && 'role' in m);
}

function TapeValue({ value, kind, direction }) {
  if (value === undefined || value === null) {
    return <div style={{ fontSize: 12, color: 'var(--ink-faint)' }}>not recorded</div>;
  }
  if (isPayloadRef(value)) {
    return kind === 'llm'
      ? <MessageList onLoadPayload={(ref) => api.payload(ref)}
        messages={[{ direction, role: undefined, offloadedMessages: true, parts: [normalizePart(value)] }]} />
      : <PayloadValue reference={value} />;
  }
  if (kind === 'llm' && isMessageList(value)) {
    return <MessageList messages={value.map((m) => tapeMessage(m, direction))} onLoadPayload={(ref) => api.payload(ref)} />;
  }
  const text = typeof value === 'string' ? value : JSON.stringify(value, null, 2);
  return <CodeBlock code={text} language={typeof value === 'string' ? undefined : 'json'} maxHeight={280} />;
}

/** An offloaded value that is not a conversation: its preview, and its bytes
    on request — the tape keeps references unless asked to inline them. */
function PayloadValue({ reference }) {
  const [state, setState] = React.useState({ phase: 'idle' });
  const load = () => {
    setState({ phase: 'loading' });
    api.payload(reference.$payload).then(
      (text) => setState({ phase: 'done', text }),
      (error) => setState({ phase: 'error', what: error.what || 'The payload fetch failed.' }),
    );
  };
  if (state.phase === 'done') {
    // A tool's argument or result is data, not a conversation: shown as JSON
    // when it parses, as text when it does not — never dressed as a chat turn.
    let code = state.text;
    let language;
    try { code = JSON.stringify(JSON.parse(state.text), null, 2); language = 'json'; } catch (e) { /* text */ }
    return <CodeBlock code={code} language={language} maxHeight={320} />;
  }
  return <div style={{ border: '1px dashed var(--hairline)', borderRadius: 'var(--radius-card)', padding: '8px 10px', display: 'grid', gap: 6 }}>
    <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap' }}>
      <Tag mono>offloaded</Tag>
      <Mono size={11} color="var(--ink-faint)">{reference.$payload.slice(0, 20)}…</Mono>
      {bytesLabel(reference.bytes) ? <Mono size={11} color="var(--ink-muted)">{bytesLabel(reference.bytes)}</Mono> : null}
      <span style={{ marginLeft: 'auto' }}>
        <Chip onClick={state.phase === 'loading' ? undefined : load}>{state.phase === 'loading' ? 'Loading…' : 'Load payload'}</Chip>
      </span>
    </div>
    {reference.preview ? <div style={{ fontFamily: 'var(--font-mono)', fontSize: 12, color: 'var(--ink-muted)', whiteSpace: 'pre-wrap', overflowWrap: 'anywhere' }}>{reference.preview}</div> : null}
    {state.phase === 'error' ? <div role="alert" style={{ fontSize: 12, color: 'var(--error)' }}>{state.what}</div> : null}
  </div>;
}

/** Which recording a replay panel is about: a run read in one scope. An
    absent tenant (every tenant's run of this id, the operator view) is a
    different scope from `''` (the default tenant's), so they key apart. */
function replayScopeKey(runId, tenant) {
  return JSON.stringify([runId, tenant === undefined ? null : tenant]);
}

/** "Replay this run": snapshot the tape into a replay and hand over the two
    environment lines each SDK needs. Creating one is a read — it changes
    nothing in the store — so any credential may, `ro` included.

    Rendered keyed by `replayScopeKey`, so a new run or tenant scope is a new
    panel: a replay of the old recording is not kept beside the new tape, and
    a creation or status read still in flight lands on the panel that asked —
    gone — rather than restoring an obsolete replay into this one. */
function ReplayPanel({ runId, tenant, tape, identity, pushToast }) {
  const [mode, setMode] = React.useState('strict');
  const [replay, setReplay] = React.useState(null);
  const [status, setStatus] = React.useState(null);
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState(null);

  const create = async () => {
    setBusy(true);
    setError(null);
    setStatus(null);
    try {
      const body = { run: runId, mode };
      // Empty names the default tenant; absent is the operator view, which
      // is what an unscoped tape on this page shows.
      if (tenant !== undefined) body.tenant = tenant;
      setReplay(await api.createReplay(body));
    } catch (e) {
      setError(e);
    } finally { setBusy(false); }
  };

  const check = async () => {
    try {
      setStatus(await api.replay(replay.id));
    } catch (e) {
      pushToast({ status: 'error', title: e.what || 'Could not read the replay', detail: e.next });
    }
  };

  return <Card>
    <PanelHead title="Replay this run" note="every model answer served from the tape; zero model calls" />
    <div style={{ fontSize: 12, color: 'var(--ink-muted)', lineHeight: '19px', marginBottom: 10 }}>
      Your agent runs for real against its own recording: point its OpenAI or Anthropic SDK at the
      replay and each model call gets the next recorded answer. Tools still execute live. A replay
      lives in server memory for one hour.
    </div>
    <div style={{ display: 'flex', gap: 6, alignItems: 'center', flexWrap: 'wrap', marginBottom: 10 }}>
      <Chip active={mode === 'strict'} onClick={() => setMode('strict')}
        title="Refuse a request whose messages differ from the recording">strict</Chip>
      <Chip active={mode === 'loose'} onClick={() => setMode('loose')}
        title="Serve the recorded answers in order, whatever is asked">loose</Chip>
      <span style={{ marginLeft: 'auto' }}>
        <Chip tone="primary" onClick={busy ? undefined : create}>{busy ? 'Creating…' : replay ? 'Create another' : 'Create replay'}</Chip>
      </span>
    </div>
    {error ? <ErrorState what={error.what} next={error.next ||
      (error.status === 409 ? 'This run cannot be replayed as recorded; the message names the step.' : undefined)} /> : null}
    {replay ? <div data-testid="replay-ready" style={{ display: 'grid', gap: 10 }}>
      <div style={{ display: 'flex', gap: 10, flexWrap: 'wrap', fontSize: 12, color: 'var(--ink-muted)' }}>
        <Mono color="var(--ink)">{replay.id}</Mono>
        <span><Mono color="var(--accent)">{replay.llm_steps}</Mono> model steps of <Mono>{replay.steps}</Mono></span>
        <span>{replay.mode}</span>
        <span>expires <Mono>{fmtTimeNs(replay.expires_at_ns)}</Mono></span>
      </div>
      {replay.unreadable_tool_steps?.length ? <div style={{ fontSize: 12, color: 'var(--warn)' }}>
        Tool steps {replay.unreadable_tool_steps.join(', ')} could not be read and will not be served by
        <code> tools/next</code>.
      </div> : null}
      <CodeBlock language="sh" code={replayEnv(replay, publicBase(), identity?.auth)} />
      <CodeBlock language="python" code={replayPython(tape)} />
      <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap' }}>
        <Chip onClick={check}>What has it served?</Chip>
        {status ? <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>
          served <Mono color="var(--accent)">{status.served}</Mono>, remaining <Mono>{status.remaining}</Mono>
          {status.diverged
            ? <span style={{ color: 'var(--error)' }}> · diverged at step {status.diverged.step}</span>
            : ' · no divergence'}
        </span> : null}
      </div>
    </div> : null}
  </Card>;
}

/** "Compare with…": another run beside this one, step by step. Offers recent
    sessions as suggestions; any run id works, `trace:<id>` included. */
function ComparePanel({ runId, tenant, go }) {
  const [other, setOther] = React.useState('');
  const recent = useRead((signal) => api.sessions({ limit: 30 }, signal).catch(() => null), []);
  // A session row omits `tenant` for the default tenant.
  const suggestions = (recent.data?.sessions || [])
    .filter((s) => s.session_id !== runId && (tenant === undefined || (s.tenant || '') === tenant));
  const compare = () => {
    const b = other.trim();
    if (!b) return;
    go(['diff'], { a: runId, b, ...tenantParams(tenant) });
  };
  return <Card>
    <PanelHead title="Compare with…" note="aligns the two tapes and names the first divergence" />
    <div style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
      <Input size="sm" mono value={other} onChange={setOther} placeholder="another run id"
        ariaLabel="Run to compare with" list="tz-run-suggestions" style={{ flex: 1 }}
        onKeyDown={(e) => { if (e.key === 'Enter') compare(); }} />
      <datalist id="tz-run-suggestions">
        {suggestions.map((s) => <option key={s.session_id} value={s.session_id} />)}
      </datalist>
      <Chip tone="primary" onClick={compare}>Compare</Chip>
    </div>
    {recent.loading ? <LoadingBar active style={{ marginTop: 8 }} /> : null}
  </Card>;
}
