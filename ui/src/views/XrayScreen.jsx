import React from 'react';
import { api } from '../lib/api.js';
import { publicBase } from '../lib/base.js';
import { navigate, useRead } from '../lib/route.js';
import { fmtDurationNs, fmtNum, fmtTimeNs } from '../lib/format.js';
import { routeTenant, tenantParams } from '../lib/runs.js';
import {
  CACHE_CAUSE, cacheSegments, fmtLostUsd, provenanceKey, stableWithoutRead,
} from '../lib/insight.js';
import { Card, Chip, EmptyState, ErrorState, LoadingBar, Mono, PanelHead, Skeleton } from '../components/primitives/Chrome.jsx';
import { Tag } from '../components/primitives/Tag.jsx';
import { Input } from '../components/primitives/Input.jsx';
import { Caveat, DiffPair, Kv, Swatch, Tile, TileRow, Tooltip } from '../components/insight/Bits.jsx';
import { ContextBar, ContextLegend, ProvenanceChips } from '../components/insight/ContextBar.jsx';

// X-Ray: a run's model calls as the contexts they were. The Cache Autopsy
// lines up every call against the one it continues and names why the
// provider's prompt cache broke, and what that cost; under it, one call's
// context window — every chunk, where it came from, what is dead weight,
// and what the previous call had that this one lost. Deterministic
// arithmetic over stored spans: no model is called to produce any of it.

const TTLS = [{ id: '300', label: '5 min' }, { id: '3600', label: '1 h' }];

export function XrayScreen({ runId, go, params }) {
  if (!runId) return <XrayLanding go={go} />;
  return <RunXray runId={runId} go={go} params={params} />;
}

function RunXray({ runId, go, params }) {
  const tenant = routeTenant(params);
  const scope = tenantParams(tenant);
  const ttl = params.get('ttl') === '3600' ? '3600' : '300';
  const stepParam = params.get('step');
  const set = (changes) => {
    const next = new URLSearchParams(params);
    for (const [key, value] of Object.entries(changes)) {
      if (value == null) next.delete(key); else next.set(key, String(value));
    }
    navigate(['xray', runId], Object.fromEntries(next), { replace: true });
  };

  const autopsy = useRead((signal) => api.cache(runId, { ...scope, ttl_seconds: ttl === '300' ? undefined : ttl }, signal),
    [runId, tenant, ttl]);
  const steps = (autopsy.data && autopsy.data.steps) || [];
  const chosen = stepParam != null && /^[0-9]+$/.test(stepParam)
    ? steps.find((s) => s.step_index === Number(stepParam)) : steps[steps.length - 1];
  const span = chosen && chosen.span;
  const xray = useRead((signal) => api.xray(span.trace_id, span.span_id,
    { ...scope, ttl_seconds: ttl === '300' ? undefined : ttl }, signal),
  [span && span.trace_id, span && span.span_id, tenant, ttl], { skip: !span });

  const pick = (stepIndex) => set({ step: stepIndex });
  const runParams = scope;

  if (autopsy.error) {
    return <ErrorState what={autopsy.error.status === 404 ? 'No spans carry this run id.' : autopsy.error.what}
      next={autopsy.error.status === 404
        ? 'A run is a session id, or trace:<trace_id> for a trace without one; its spans may belong to another tenant.'
        : autopsy.error.next} onRetry={autopsy.reload} />;
  }
  if (!autopsy.data) return <Skeleton height={300} />;
  const summary = autopsy.data.summary;

  return <div style={{ display: 'grid', gap: 14, maxWidth: 1800 }}>
    <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
      <Mono size={13}>{runId}</Mono>
      {typeof autopsy.data.tenant === 'string' && autopsy.data.tenant ? <Mono color="var(--ink-muted)">tenant {autopsy.data.tenant}</Mono> : null}
      <span style={{ marginLeft: 'auto', display: 'flex', gap: 6, alignItems: 'center', flexWrap: 'wrap' }}>
        <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>cache lifetime</span>
        {TTLS.map((t) => <Chip key={t.id} mono active={ttl === t.id} onClick={() => set({ ttl: t.id === '300' ? null : t.id })}
          title="The TTL the verdicts assume: 5 minutes is Anthropic's default and the low end of OpenAI's">{t.label}</Chip>)}
        <Chip onClick={() => go(['run', runId], runParams)}>Run</Chip>
      </span>
    </div>
    <LoadingBar active={autopsy.loading || xray.loading} />

    <TileRow columns={6}>
      <Tile label="model calls" value={fmtNum(summary.steps)} tone="var(--ink)" />
      <Tile label="cache breaks" value={fmtNum(summary.breaks)} tone={summary.breaks ? 'var(--error)' : 'var(--ink)'} testId="tile-breaks" />
      <Tile label="est. tokens lost" value={fmtNum(summary.est_tokens_lost)} />
      <Tile label="est. cost lost USD" value={fmtLostUsd(summary.est_cost_lost_usd)}
        title={summary.est_cost_lost_usd == null ? 'The pricing table names no input and cache-read rate for these models' : 'Tokens lost at the input rate less the cache-read rate'}
        note={summary.unpriced_breaks ? `${summary.unpriced_breaks} unpriced` : ''} testId="tile-cost" />
      <Tile label="provider disagrees" value={fmtNum(summary.disagreements)} tone={summary.disagreements ? 'var(--warn)' : 'var(--ink)'}
        title="Calls where the provider's cache report contradicts the recorded prompts" />
      <Tile label="cache lifetime assumed" value={autopsy.data.ttl_seconds % 3600 === 0 ? `${autopsy.data.ttl_seconds / 3600} h` : `${Math.round(autopsy.data.ttl_seconds / 60)} min`} tone="var(--ink)" />
    </TileRow>
    {autopsy.data.truncated ? <Caveat>The run is longer than one autopsy reads (20,000 spans or 2,000 model calls); later calls are not here.</Caveat> : null}
    {autopsy.data.resolved_partial ? <Caveat>Some offloaded requests stayed unresolved, so their calls read as unknown.</Caveat> : null}
    {autopsy.data.texts_partial ? <Caveat>Over 32 MiB of distinct chunk text: some diffs were not kept.</Caveat> : null}

    {!steps.length ? <EmptyState message="This run has no model calls, so there is no context to read: the X-Ray reads llm steps." /> : <>
      <Card>
        <PanelHead title="Cache Autopsy" note="every model call against the one it continues · click a call for its X-Ray" />
        <CacheTimeline steps={steps} selected={chosen && chosen.step_index} onPick={pick} />
        {summary.disagreement_advice ? <div style={{ marginTop: 10, fontSize: 12, color: 'var(--ink-muted)', lineHeight: '18px' }}>
          <Tag status="warn" mono>provider disagrees ×{summary.disagreements}</Tag> {summary.disagreement_advice}
        </div> : null}
      </Card>
      {summary.culprits.length ? <Culprits summary={summary} onPick={pick} /> : null}
      <CallList steps={steps} selected={chosen && chosen.step_index} onPick={pick} />
    </>}

    {chosen ? <XrayView xray={xray} step={chosen} runId={runId} go={go} runParams={runParams} onPick={pick} /> : null}
  </div>;
}

/** One column per model call: its context split into the prefix the cache
    served, the tokens it re-processed although the previous call had them
    (the loss, in the accent), and what was new. */
export function CacheTimeline({ steps, selected, onPick }) {
  const wrap = React.useRef(null);
  const [hover, setHover] = React.useState(null);
  const max = Math.max(1, ...steps.map((s) => s.est_tokens || 0));
  const height = 110;
  const col = Math.max(10, Math.min(34, Math.floor(1100 / Math.max(1, steps.length))));
  const hovered = hover ? steps.find((s) => s.step_index === hover.step) : null;
  return <div ref={wrap} style={{ position: 'relative' }} onMouseLeave={() => setHover(null)}>
    <div className="tz-scroll-x">
      <div role="list" aria-label="Model calls" style={{ display: 'flex', alignItems: 'flex-end', gap: 3, minWidth: 'min-content', paddingBottom: 2 }}>
        {steps.map((step) => {
          const seg = cacheSegments(step);
          const cause = CACHE_CAUSE[step.cause] || CACHE_CAUSE.unknown;
          const active = step.step_index === selected;
          const scale = (v) => (v / max) * height;
          return <button key={step.step_index} type="button" role="listitem" className="tz-plain-button" data-testid="cache-call"
            data-cause={step.cause} aria-pressed={active}
            aria-label={`step ${step.step_index}: ${cause.label}, ${step.est_tokens} estimated tokens, ${step.est_tokens_lost} lost`}
            onClick={() => onPick(step.step_index)}
            onMouseMove={(e) => {
              const box = wrap.current?.getBoundingClientRect();
              if (box) setHover({ step: step.step_index, x: e.clientX - box.left, y: e.clientY - box.top, width: box.width });
            }}
            style={{
              width: col, flex: 'none', display: 'flex', flexDirection: 'column', alignItems: 'stretch', gap: 3,
              cursor: 'pointer', padding: '4px 2px 3px', borderRadius: 3,
              background: active ? 'var(--bg-sunken)' : 'transparent',
              boxShadow: active ? 'inset 0 -2px 0 var(--accent)' : 'none',
            }}>
            <span style={{ height, display: 'flex', flexDirection: 'column', justifyContent: 'flex-end', gap: 1 }}>
              {seg.fresh ? <span style={{ height: Math.max(1, scale(seg.fresh)), background: 'var(--series-4)', borderRadius: '1.5px 1.5px 0 0' }} /> : null}
              {seg.lost ? <span style={{ height: Math.max(2, scale(seg.lost)), background: 'var(--accent)', borderRadius: 1 }} /> : null}
              {seg.served ? <span style={{ height: Math.max(1, scale(seg.served)), background: 'var(--series-2)', borderRadius: 1 }} /> : null}
            </span>
            <span aria-hidden="true" style={{
              height: 6, borderRadius: 1.5,
              background: cause.tone === 'ok' ? 'var(--ok)' : cause.tone === 'error' ? 'var(--error)' : cause.tone === 'warn' ? 'var(--warn)' : 'var(--hairline)',
            }} />
            <span style={{ fontFamily: 'var(--font-mono)', fontSize: 10, color: active ? 'var(--ink)' : 'var(--ink-faint)', textAlign: 'center', fontVariantNumeric: 'tabular-nums' }}>
              {steps.length > 60 && step.step_index % 5 !== 0 && !active ? '' : step.step_index}
            </span>
          </button>;
        })}
      </div>
    </div>
    <div style={{ display: 'flex', flexWrap: 'wrap', gap: '4px 14px', fontSize: 12, color: 'var(--ink-muted)', marginTop: 8 }}>
      <span style={{ display: 'inline-flex', alignItems: 'center', gap: 5 }}><Swatch color="var(--series-2)" />prefix the cache could serve</span>
      <span style={{ display: 'inline-flex', alignItems: 'center', gap: 5 }}><Swatch color="var(--accent)" />re-processed: the loss</span>
      <span style={{ display: 'inline-flex', alignItems: 'center', gap: 5 }}><Swatch color="var(--series-4)" />new content</span>
      <span style={{ display: 'inline-flex', alignItems: 'center', gap: 5 }}><Swatch color="var(--ok)" />stable</span>
      <span style={{ display: 'inline-flex', alignItems: 'center', gap: 5 }}><Swatch color="var(--error)" />broken</span>
      <span style={{ display: 'inline-flex', alignItems: 'center', gap: 5 }}><Swatch color="var(--warn)" />ttl expired</span>
      <span style={{ display: 'inline-flex', alignItems: 'center', gap: 5 }}><Swatch color="var(--hairline)" />new context</span>
      <span style={{ color: 'var(--ink-faint)' }}>estimated tokens, about four characters each</span>
    </div>
    {hovered ? <Tooltip x={hover.x} y={hover.y} containerWidth={hover.width} width={320}>
      <div style={{ display: 'grid', gap: 3 }}>
        <div style={{ display: 'flex', gap: 6, alignItems: 'center' }}>
          <Mono>step {hovered.step_index}</Mono>
          <Tag status={(CACHE_CAUSE[hovered.cause] || CACHE_CAUSE.unknown).tone} mono>{(CACHE_CAUSE[hovered.cause] || CACHE_CAUSE.unknown).label}</Tag>
        </div>
        <Kv k="model">{hovered.model || '—'}</Kv>
        <Kv k="est. context">{fmtNum(hovered.est_tokens)} tokens, {fmtNum(hovered.chunks)} chunks</Kv>
        <Kv k="shared prefix">{fmtNum(hovered.shared_est_tokens)} tokens</Kv>
        <Kv k="est. lost">{fmtNum(hovered.est_tokens_lost)} tokens · ${fmtLostUsd(hovered.est_cost_lost_usd)}</Kv>
        <Kv k="provider cache read">{hovered.cache_read == null ? 'not reported' : fmtNum(hovered.cache_read)}</Kv>
        {hovered.gap_ns != null ? <Kv k="since its predecessor began">{fmtDurationNs(hovered.gap_ns)}</Kv> : null}
        <div style={{ marginTop: 3, color: 'var(--ink-muted)', fontSize: 12, overflowWrap: 'anywhere' }}>{hovered.explanation}</div>
      </div>
    </Tooltip> : null}
  </div>;
}

/** Break shapes, costliest first, each with the fixed advice the server
    chose for it. */
function Culprits({ summary, onPick }) {
  return <div className="tz-reflow-1" style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(320px, 1fr))', gap: 14 }}>
    {summary.culprits.map((c, i) => {
      const cause = CACHE_CAUSE[c.cause] || CACHE_CAUSE.unknown;
      return <Card key={i} data-testid="culprit">
        <div style={{ display: 'flex', alignItems: 'center', gap: 6, flexWrap: 'wrap', marginBottom: 8 }}>
          <Tag status={cause.tone} mono>{cause.label}</Tag>
          {c.change ? <Tag mono>{c.change}</Tag> : null}
          {c.role ? <Tag mono>{c.role}</Tag> : null}
          {c.volatile ? <Tag status="accent" mono>{c.volatile}</Tag> : null}
          <span style={{ marginLeft: 'auto', fontSize: 12, color: 'var(--ink-muted)' }}>
            <Mono color="var(--ink)">{fmtNum(c.breaks)}</Mono> break{c.breaks === 1 ? '' : 's'} · <Mono color="var(--accent)">~{fmtNum(c.est_tokens_lost)}</Mono> tokens · <Mono color="var(--accent)">${fmtLostUsd(c.est_cost_lost_usd)}</Mono>
          </span>
        </div>
        {c.example ? <div style={{ marginBottom: 8 }}><DiffPair diff={c.example} /></div> : null}
        <div style={{ fontSize: 13, lineHeight: '20px', color: 'var(--ink)' }}>{c.advice}</div>
        {typeof c.example_step_index === 'number' ? <div style={{ marginTop: 8 }}>
          <Chip onClick={() => onPick(c.example_step_index)}>See step {c.example_step_index}</Chip>
        </div> : null}
      </Card>;
    })}
    {summary.culprits_truncated ? <Caveat>More break shapes than the ten listed.</Caveat> : null}
  </div>;
}

/** Every call's verdict as a line, breaks first by default. */
function CallList({ steps, selected, onPick }) {
  const breaks = steps.filter((s) => s.cause !== 'stable' && s.previous_step_index != null);
  const [all, setAll] = React.useState(!breaks.length);
  const shown = all ? steps : breaks;
  return <Card pad="0">
    <div style={{ padding: '12px 14px 8px', display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
      <PanelHead title="Calls" note="the verdict, the first chunk that changed, and what the provider reported" style={{ marginBottom: 0 }} />
      <span style={{ marginLeft: 'auto', display: 'flex', gap: 6 }}>
        <Chip active={!all} onClick={() => setAll(false)}>breaks ({breaks.length})</Chip>
        <Chip active={all} onClick={() => setAll(true)}>all ({steps.length})</Chip>
      </span>
    </div>
    <div className="tz-scroll-x">
      <div role="table" aria-label="Cache verdicts" style={{ minWidth: 760 }}>
        {shown.map((step) => {
          const cause = CACHE_CAUSE[step.cause] || CACHE_CAUSE.unknown;
          const d = step.first_divergent_chunk;
          const active = step.step_index === selected;
          return <div key={step.step_index} role="row" tabIndex={0} onClick={() => onPick(step.step_index)}
            onKeyDown={(e) => { if (e.key === 'Enter') onPick(step.step_index); }}
            style={{
              display: 'grid', gridTemplateColumns: '58px 132px minmax(0, 1fr) 96px 120px', gap: 10, alignItems: 'start',
              padding: '8px 14px', borderTop: '1px solid var(--hairline)', cursor: 'pointer', fontSize: 12,
              background: active ? 'var(--bg-sunken)' : 'transparent', borderLeft: `2px solid ${active ? 'var(--accent)' : 'transparent'}`,
            }}>
            <div role="cell"><Mono color="var(--ink-muted)">step {step.step_index}</Mono></div>
            <div role="cell" style={{ display: 'grid', gap: 3, justifyItems: 'start' }}>
              <Tag status={cause.tone} mono>{cause.label}</Tag>
              {d ? <Mono size={11} color="var(--ink-faint)">{d.change}{d.volatile ? ' · ' + d.volatile : ''}{d.role ? ' · ' + d.role : ''}</Mono> : null}
            </div>
            <div role="cell" style={{ display: 'grid', gap: 4, minWidth: 0 }}>
              <span style={{ color: 'var(--ink)', lineHeight: '18px', overflowWrap: 'anywhere' }}>{step.explanation}</span>
              {d && d.diff ? <DiffPair diff={d.diff} /> : null}
            </div>
            <div role="cell" style={{ textAlign: 'right' }}>
              <Mono color={step.est_tokens_lost ? 'var(--accent)' : 'var(--ink-faint)'}>{fmtNum(step.est_tokens_lost)}</Mono>
              <div><Mono size={11} color="var(--ink-faint)">${fmtLostUsd(step.est_cost_lost_usd)}</Mono></div>
            </div>
            <div role="cell" style={{ textAlign: 'right' }}>
              <Mono color="var(--ink-muted)">{step.cache_read == null ? 'no report' : `read ${fmtNum(step.cache_read)}`}</Mono>
              {stableWithoutRead(step) ? <div><Mono size={11} color="var(--warn)">disagrees</Mono></div> : null}
            </div>
          </div>;
        })}
        {!shown.length ? <div style={{ padding: '10px 14px', borderTop: '1px solid var(--hairline)', fontSize: 12, color: 'var(--ink-muted)' }}>
          No call broke the cache: every call with a predecessor kept its prefix.
        </div> : null}
      </div>
    </div>
  </Card>;
}

const FILTERS = [
  { id: 'all', label: 'all' },
  { id: 'stale', label: 'stale' },
  { id: 'dup', label: 'repeats' },
  { id: 'tool_result', label: 'tool results' },
  { id: 'instructions', label: 'instructions' },
];

function passes(chunk, filter) {
  if (filter === 'stale') return chunk.stale;
  if (filter === 'dup') return chunk.duplicate_of != null;
  if (filter === 'tool_result') return chunk.kind === 'tool_result';
  if (filter === 'instructions') return chunk.kind === 'system' || chunk.kind === 'user';
  return true;
}

/** One call's X-Ray. */
function XrayView({ xray, step, runId, go, runParams, onPick }) {
  const [focusKey, setFocusKey] = React.useState(null);
  const [fromKey, setFromKey] = React.useState(null);
  const [filter, setFilter] = React.useState('all');
  const [picked, setPicked] = React.useState(null);
  React.useEffect(() => { setFromKey(null); setFilter('all'); setPicked(null); }, [step.step_index]);

  if (xray.error) {
    return <ErrorState what={xray.error.what} next={xray.error.next} onRetry={xray.reload} />;
  }
  if (!xray.data) return <Skeleton height={320} />;
  const x = xray.data;
  const chunks = x.chunks || [];
  const shown = chunks.filter((c) => passes(c, filter) && (!fromKey || provenanceKey(c) === fromKey));
  const toTape = (index) => go(['run', runId], { ...(runParams || {}), step: String(index) });
  const caveats = [];
  if (x.context_unavailable) caveats.push(`The context ${x.context_unavailable}.`);
  if (x.chunks_truncated) caveats.push('The context had more than 2,000 chunks; the rest is not analysed.');
  if (x.provenance_partial) caveats.push('A provenance or staleness bound was reached: introduced-by and stale are incomplete.');
  if (x.texts_partial) caveats.push('Over 32 MiB of distinct chunk text: some previews and diffs were not kept.');
  if (x.resolved_partial) caveats.push('An offloaded value stayed unresolved.');

  return <section aria-label={`X-Ray of step ${x.step_index}`} style={{ display: 'grid', gap: 14 }} data-testid="xray">
    <div style={{ display: 'flex', alignItems: 'baseline', gap: 10, flexWrap: 'wrap', paddingTop: 6, borderTop: '1px solid var(--hairline)' }}>
      <h2 style={{ margin: 0, fontSize: 16, fontWeight: 600 }}>X-Ray · step {x.step_index ?? '—'}</h2>
      <Mono color="var(--ink-muted)">{x.model || 'model not recorded'}</Mono>
      {x.provider ? <Mono color="var(--ink-faint)">{x.provider}</Mono> : null}
      <Mono size={11} color="var(--ink-faint)">{fmtTimeNs(x.start_ns)}</Mono>
      {x.previous ? <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>
        continues <a href="#" onClick={(e) => { e.preventDefault(); onPick(x.previous.step_index); }}><Mono>step {x.previous.step_index}</Mono></a>
        {' '}(<Mono>{fmtNum(x.previous.shared_chunks)}</Mono> chunks shared)
      </span> : <span style={{ fontSize: 12, color: 'var(--ink-faint)' }}>no previous call in its conversation</span>}
      <span style={{ marginLeft: 'auto', display: 'flex', gap: 6 }}>
        {typeof x.step_index === 'number' ? <Chip onClick={() => toTape(x.step_index)}>On the tape</Chip> : null}
        <Chip onClick={() => go(['trace', x.span.trace_id], { span: x.span.span_id })}>Span in its trace</Chip>
      </span>
    </div>

    <TileRow columns={6}>
      <Tile label="est. context tokens" value={fmtNum(x.totals.est_tokens)} testId="tile-est" />
      <Tile label="provider context" value={x.totals.reported_context_tokens == null ? '—' : fmtNum(x.totals.reported_context_tokens)} tone="var(--ink)"
        title="The context resolved across the providers' cache conventions; reported input tokens alone may be the uncached remainder" />
      <Tile label="provider cache read" value={x.totals.cache_read == null ? '—' : fmtNum(x.totals.cache_read)} tone="var(--ink)" />
      <Tile label="stale" value={fmtNum(x.totals.stale_est_tokens)} note={`${x.totals.stale_chunks} chunks`}
        title={`Tool results older than ${x.stale_after_steps} steps that no model output since drew on — a heuristic`} />
      <Tile label="repeated" value={fmtNum(x.totals.duplicate_est_tokens)} title="Tokens of every copy after the first" />
      <Tile label="chunks" value={fmtNum(x.totals.chunks)} tone="var(--ink)" />
    </TileRow>
    {caveats.length ? <div style={{ display: 'grid', gap: 3 }}>{caveats.map((c) => <Caveat key={c}>{c}</Caveat>)}</div> : null}

    {chunks.length ? <Card>
      <PanelHead title="Context window" note="as the model read it, left to right · width is estimated tokens · hover for provenance" />
      <ContextBar chunks={chunks} focusKey={focusKey || fromKey} onFocus={setFocusKey}
        onPick={(chunk) => { setPicked(chunk.index); setFilter('all'); setFromKey(null); }} />
      <div style={{ display: 'grid', gap: 8, marginTop: 10 }}>
        <ContextLegend chunks={chunks} />
        <ProvenanceChips chunks={chunks} focusKey={focusKey} onFocus={setFocusKey} filterKey={fromKey} onFilter={setFromKey} />
      </div>
    </Card> : null}

    <div className="tz-split" style={{ display: 'grid', gridTemplateColumns: 'minmax(0, 1fr) minmax(320px, 440px)', gap: 14, alignItems: 'start' }}>
      <ChunkList chunks={shown} total={chunks.length} filter={filter} setFilter={setFilter} fromKey={fromKey} clearFrom={() => setFromKey(null)}
        picked={picked} staleAfter={x.stale_after_steps} toTape={toTape} focusKey={focusKey} />
      <div style={{ display: 'grid', gap: 14, minWidth: 0 }}>
        <Dropped dropped={x.dropped_since_previous} truncated={x.dropped_truncated} toTape={toTape} previous={x.previous} />
        <Edited edited={x.edited_since_previous} truncated={x.edited_truncated} />
        <CallVerdict cache={x.cache} />
        <Duplicates duplicates={x.duplicates} truncated={x.duplicates_truncated} onPick={(i) => { setPicked(i); setFilter('all'); setFromKey(null); }} />
        <Composition composition={x.composition} />
        <Response response={x.response} />
      </div>
    </div>
  </section>;
}

function ChunkList({ chunks, total, filter, setFilter, fromKey, clearFrom, picked, staleAfter, toTape, focusKey }) {
  const pickedRef = React.useRef(null);
  React.useEffect(() => {
    if (picked != null && pickedRef.current && pickedRef.current.scrollIntoView) pickedRef.current.scrollIntoView({ block: 'nearest' });
  }, [picked]);
  return <Card pad="0">
    <div style={{ padding: '12px 14px 8px', display: 'flex', alignItems: 'center', gap: 6, flexWrap: 'wrap' }}>
      <PanelHead title="Chunks" note={`${chunks.length} of ${total} · stale after ${staleAfter} steps`} style={{ marginBottom: 0 }} />
      <span style={{ marginLeft: 'auto', display: 'flex', gap: 6, flexWrap: 'wrap' }}>
        {fromKey ? <Chip active onClick={clearFrom} title="Show chunks from every step">{fromKey === 'self' ? 'new in this call' : 'step ' + fromKey.slice(1)} ×</Chip> : null}
        {FILTERS.map((f) => <Chip key={f.id} active={filter === f.id} onClick={() => setFilter(f.id)}>{f.label}</Chip>)}
      </span>
    </div>
    <div className="tz-scroll-x">
      <div role="table" aria-label="Context chunks" style={{ minWidth: 640 }}>
        <div role="row" style={{ display: 'grid', gridTemplateColumns: COLUMNS, background: 'var(--bg-sunken)', borderTop: '1px solid var(--hairline)', borderBottom: '1px solid var(--hairline)' }}>
          {['#', 'kind', 'tokens', 'introduced by', 'age', 'content'].map((h, i) => <div key={h} role="columnheader" style={{
            padding: '5px 8px', fontSize: 12, fontWeight: 500, color: 'var(--ink-muted)', textAlign: i === 2 || i === 4 ? 'right' : 'left',
          }}>{h}</div>)}
        </div>
        {chunks.map((chunk) => <ChunkRow key={chunk.index} chunk={chunk} picked={picked === chunk.index}
          rowRef={picked === chunk.index ? pickedRef : undefined} toTape={toTape} lit={focusKey && provenanceKey(chunk) === focusKey} />)}
        {!chunks.length ? <div style={{ padding: '10px 14px', fontSize: 12, color: 'var(--ink-muted)' }}>No chunk matches.</div> : null}
      </div>
    </div>
  </Card>;
}

const COLUMNS = '40px 150px 58px 170px 40px minmax(0, 1fr)';

function ChunkRow({ chunk, picked, rowRef, toTape, lit }) {
  const [open, setOpen] = React.useState(false);
  const by = chunk.introduced_by;
  return <div ref={rowRef} role="row" data-testid="chunk-row" data-index={chunk.index} tabIndex={0}
    onClick={() => setOpen((v) => !v)} onKeyDown={(e) => { if (e.key === 'Enter') setOpen((v) => !v); }}
    style={{
      display: 'grid', gridTemplateColumns: COLUMNS, alignItems: 'start', borderBottom: '1px solid var(--hairline)',
      fontSize: 'var(--cell-fs)', cursor: 'pointer',
      background: picked ? 'var(--accent-tint)' : lit ? 'var(--bg-sunken)' : 'transparent',
    }}>
    <div role="cell" style={{ padding: 'var(--row-py) 8px', position: 'relative', alignSelf: 'stretch' }}>
      {chunk.stale ? <span aria-hidden="true" style={{
        position: 'absolute', left: 0, top: 0, bottom: 0, width: 4,
        background: 'repeating-linear-gradient(135deg, var(--series-2) 0 2px, transparent 2px 4px)',
      }} /> : null}
      <Mono color="var(--ink-faint)">{chunk.index}</Mono>
    </div>
    <div role="cell" style={{ padding: 'var(--row-py) 8px', display: 'flex', gap: 6, alignItems: 'center', minWidth: 0 }}>
      <Tag mono>{chunk.kind}</Tag>
      {chunk.name ? <Mono size={11} color="var(--ink-muted)" style={{ overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{chunk.name}</Mono> : null}
    </div>
    <div role="cell" style={{ padding: 'var(--row-py) 8px', textAlign: 'right' }}><Mono color="var(--accent)">{fmtNum(chunk.est_tokens)}</Mono></div>
    <div role="cell" style={{ padding: 'var(--row-py) 8px', minWidth: 0, whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis' }}
      title={by ? `${by.kind}${by.name ? ' ' + by.name : ''}${by.via === 'tool_call_id' ? ' — matched by tool-call id: the model was shown something other than what the tool returned' : ''}` : 'No earlier step held this content'}>
      {by && typeof by.step_index === 'number' ? <a href="#" onClick={(e) => { e.preventDefault(); e.stopPropagation(); toTape(by.step_index); }}
        style={{ fontFamily: 'var(--font-mono)', fontSize: 12 }}>step {by.step_index}</a> : <Mono color="var(--ink-faint)">this call</Mono>}
      {by ? <span style={{ fontSize: 11, color: 'var(--ink-faint)', marginLeft: 6 }}>{by.kind}{by.name ? ' ' + by.name : ''}{by.via === 'tool_call_id' ? ' · by call id' : ''}</span> : null}
    </div>
    <div role="cell" style={{ padding: 'var(--row-py) 8px', textAlign: 'right' }}><Mono color="var(--ink-muted)">{chunk.age_steps ?? '—'}</Mono></div>
    <div role="cell" style={{ padding: 'var(--row-py) 8px', minWidth: 0, display: 'flex', gap: 6, alignItems: open ? 'flex-start' : 'center' }}
      title={chunk.tool_call_id ? 'tool call ' + chunk.tool_call_id : undefined}>
      {chunk.stale ? <Tag mono status="warn">stale</Tag> : null}
      {chunk.duplicate_of != null ? <Tag mono status="warn">repeat of #{chunk.duplicate_of}</Tag> : null}
      <div style={{
        fontFamily: 'var(--font-mono)', fontSize: 12, color: 'var(--ink)', minWidth: 0, flex: 1,
        whiteSpace: open ? 'pre-wrap' : 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis', overflowWrap: open ? 'anywhere' : undefined,
      }}>{chunk.preview}</div>
    </div>
  </div>;
}

/** What the previous call had and this one does not. A lost system or user
    chunk is an instruction the agent may no longer see. */
function Dropped({ dropped, truncated, toTape, previous }) {
  const [all, setAll] = React.useState(false);
  const list = [...(dropped || [])].sort((a, b) => (a.kind === 'instruction' ? 0 : 1) - (b.kind === 'instruction' ? 0 : 1));
  const instructions = list.filter((d) => d.kind === 'instruction' && !d.unverified).length;
  // Every instruction is shown; the rest of a compaction's losses fold
  // behind a count, since a rewrite can drop the whole history.
  const others = list.filter((d) => d.kind !== 'instruction');
  const shown = all ? list : [...list.filter((d) => d.kind === 'instruction'), ...others.slice(0, 3)];
  return <Card data-testid="dropped" style={{ borderColor: instructions ? 'var(--error)' : 'var(--hairline)' }}>
    <PanelHead title="Dropped since the previous call"
      note={previous ? `against step ${previous.step_index}` : 'no previous call'} />
    {!list.length ? <div style={{ fontSize: 12, color: 'var(--ink-muted)' }}>{previous ? 'Nothing the previous call had is missing.' : 'This call starts its conversation.'}</div>
      : <div style={{ display: 'grid', gap: 8 }}>
        {instructions ? <div style={{ fontSize: 13, color: 'var(--error)', fontWeight: 500 }}>
          {instructions} instruction{instructions === 1 ? '' : 's'} the agent was given {instructions === 1 ? 'is' : 'are'} no longer in its context.
        </div> : null}
        {shown.map((d, i) => <DroppedChunk key={d.digest + i} d={d} toTape={toTape} />)}
        {!all && list.length > shown.length ? <div><Chip onClick={() => setAll(true)}>{list.length - shown.length} more dropped chunk{list.length - shown.length === 1 ? '' : 's'}</Chip></div> : null}
        {truncated ? <Caveat>More than 256 dropped chunks; the rest are not listed.</Caveat> : null}
      </div>}
  </Card>;
}

function DroppedChunk({ d, toTape }) {
  const [open, setOpen] = React.useState(d.kind === 'instruction');
  return <div style={{
    display: 'grid', gap: 4, padding: '7px 9px', borderRadius: 'var(--radius-control)',
    background: d.kind === 'instruction' ? 'var(--error-tint)' : 'var(--bg-sunken)',
  }}>
    <div style={{ display: 'flex', gap: 6, alignItems: 'center', flexWrap: 'wrap' }}>
      {d.kind === 'instruction' ? <Tag status="error" mono>instruction lost</Tag> : <Tag mono>{d.chunk_kind}</Tag>}
      {d.unverified ? <Tag status="warn" mono>unverified: may be edited</Tag> : null}
      <Mono size={11} color="var(--ink-muted)">{d.role || ''} · ~{fmtNum(d.est_tokens)} tokens</Mono>
      {d.introduced_by && typeof d.introduced_by.step_index === 'number' ? <a href="#" style={{ marginLeft: 'auto', fontSize: 12 }}
        onClick={(e) => { e.preventDefault(); toTape(d.introduced_by.step_index); }}>from step {d.introduced_by.step_index}</a> : null}
    </div>
    <div onClick={() => setOpen((v) => !v)} style={{
      fontFamily: 'var(--font-mono)', fontSize: 12, whiteSpace: open ? 'pre-wrap' : 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis',
      overflowWrap: 'anywhere', color: d.preview == null ? 'var(--ink-faint)' : 'var(--ink)', cursor: 'pointer',
    }}>
      {d.preview == null ? 'the text was not kept' : d.preview}
    </div>
  </div>;
}

function Edited({ edited, truncated }) {
  if (!edited || !edited.length) return null;
  return <Card>
    <PanelHead title="Edited since the previous call" note="the same chunk, changed in place" />
    <div style={{ display: 'grid', gap: 8 }}>
      {edited.map((e) => <div key={e.index} style={{ display: 'grid', gap: 4 }}>
        <div style={{ display: 'flex', gap: 6, alignItems: 'center' }}>
          <Mono size={11} color="var(--ink-muted)">chunk {e.index} · {e.role}</Mono>
          {e.volatile ? <Tag status="accent" mono>{e.volatile}</Tag> : null}
        </div>
        <DiffPair diff={e.diff} />
      </div>)}
      {truncated ? <Caveat>More than 256 edited chunks.</Caveat> : null}
    </div>
  </Card>;
}

function CallVerdict({ cache }) {
  if (!cache) return null;
  const cause = CACHE_CAUSE[cache.cause] || CACHE_CAUSE.unknown;
  const d = cache.first_divergent_chunk;
  return <Card>
    <PanelHead title="This call's cache" note={cache.previous_step_index != null ? `against step ${cache.previous_step_index}` : ''} />
    <div style={{ display: 'grid', gap: 8 }}>
      <div style={{ display: 'flex', gap: 6, alignItems: 'center', flexWrap: 'wrap' }}>
        <Tag status={cause.tone} mono>{cause.label}</Tag>
        {d ? <Mono size={11} color="var(--ink-muted)">chunk {d.index} {d.change}{d.volatile ? ' · ' + d.volatile : ''}</Mono> : null}
      </div>
      <div style={{ fontSize: 12, lineHeight: '18px', overflowWrap: 'anywhere' }}>{cache.explanation}</div>
      {d && d.diff ? <DiffPair diff={d.diff} /> : null}
      <div style={{ display: 'grid', gap: 3, fontSize: 12 }}>
        <Kv k="prefix shared">{fmtNum(cache.prefix_chunks_shared)} chunks · ~{fmtNum(cache.shared_est_tokens)} tokens</Kv>
        <Kv k="est. lost">{fmtNum(cache.est_tokens_lost)} tokens · ${fmtLostUsd(cache.est_cost_lost_usd)}</Kv>
        <Kv k="provider">{cache.cache_read == null ? 'no cache report' : `read ${fmtNum(cache.cache_read)}${cache.reported_hit ? ' (hit)' : ''}`}</Kv>
        {cache.calibration != null ? <Kv k="calibration" title="The provider's context count over the estimate; scales the loss">{cache.calibration.toFixed(2)}×</Kv> : null}
      </div>
    </div>
  </Card>;
}

function Duplicates({ duplicates, truncated, onPick }) {
  if (!duplicates || !duplicates.length) return null;
  return <Card>
    <PanelHead title="Repeated content" note="in the context more than once" />
    <div style={{ display: 'grid', gap: 8 }}>
      {duplicates.map((d) => <div key={d.digest} style={{ display: 'grid', gap: 3 }}>
        <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap', fontSize: 12 }}>
          <Mono color="var(--warn)">×{d.count}</Mono>
          <Mono size={11} color="var(--ink-muted)">~{fmtNum(d.est_tokens_each)} tokens each</Mono>
          <span style={{ display: 'flex', gap: 4 }}>{d.indices.map((i) => <Chip key={i} mono onClick={() => onPick(i)}>#{i}</Chip>)}</span>
        </div>
        <div style={{ fontFamily: 'var(--font-mono)', fontSize: 12, whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis' }}>{d.preview}</div>
      </div>)}
      {truncated ? <Caveat>More than 64 groups of repeats.</Caveat> : null}
    </div>
  </Card>;
}

function Composition({ composition }) {
  const rows = (composition && composition.by_source) || [];
  if (!rows.length) return null;
  return <Card>
    <PanelHead title="Where the context came from" note="estimated tokens by source" />
    <div style={{ display: 'grid', gap: 5 }}>
      {rows.map((r) => <div key={r.key} style={{ display: 'grid', gridTemplateColumns: 'minmax(0, 1fr) 110px 48px', gap: 8, alignItems: 'center', fontSize: 12 }}>
        <Mono size={12} style={{ overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{r.key}</Mono>
        <span style={{ height: 6, background: 'var(--bg-sunken)', borderRadius: 1.5, position: 'relative' }}>
          <span style={{ position: 'absolute', left: 0, top: 0, bottom: 0, width: Math.max(1, r.share * 100) + '%', background: 'var(--accent)', borderRadius: 1.5 }} />
        </span>
        <Mono size={12} color="var(--accent)" style={{ textAlign: 'right' }}>{Math.round(r.share * 100)}%</Mono>
      </div>)}
    </div>
  </Card>;
}

function Response({ response }) {
  if (!response || !response.length) return null;
  return <Card>
    <PanelHead title="What the model answered" />
    <div style={{ display: 'grid', gap: 6 }}>
      {response.map((r) => <div key={r.index} style={{ display: 'grid', gap: 2 }}>
        <Mono size={11} color="var(--ink-muted)">{r.kind}{r.name ? ' · ' + r.name : ''} · ~{fmtNum(r.est_tokens)} tokens</Mono>
        <div style={{ fontFamily: 'var(--font-mono)', fontSize: 12, whiteSpace: 'pre-wrap', overflowWrap: 'anywhere' }}>{r.preview}</div>
      </div>)}
    </div>
  </Card>;
}

/** No run named: say what the screen reads, take a run id, and offer the
    recent sessions that made model calls. */
function XrayLanding({ go }) {
  const [draft, setDraft] = React.useState('');
  const recent = useRead((signal) => api.sessions({ limit: 50 }, signal), []);
  const rows = ((recent.data && recent.data.sessions) || []).filter((s) => s.llm_calls > 0);
  const open = (id, tenant) => go(['xray', id], typeof tenant === 'string' ? { tenant } : undefined);
  return <div style={{ display: 'grid', gridTemplateColumns: 'minmax(0, 1fr)', gap: 14, maxWidth: 1100 }}>
    <Card>
      <PanelHead title="Open a run" note="a session id, or trace:<trace_id>" />
      <div style={{ fontSize: 13, color: 'var(--ink-muted)', lineHeight: '20px', marginBottom: 10, textWrap: 'pretty' }}>
        The X-Ray reads one model call's request as the context window it was — every chunk, the step that
        introduced it, what is stale or repeated, what the previous call had that this one lost — and the Cache
        Autopsy lines every call up against the one it continues to say why the prompt cache broke and what it
        cost. Open it from any model call on a run's tape, or name the run here.
      </div>
      <div style={{ display: 'flex', gap: 8 }}>
        <Input mono value={draft} onChange={setDraft} placeholder="run id" ariaLabel="Run id" style={{ flex: 1 }}
          onKeyDown={(e) => { if (e.key === 'Enter' && draft.trim()) open(draft.trim()); }} />
        <Chip tone="primary" onClick={() => draft.trim() && open(draft.trim())}>Open</Chip>
      </div>
    </Card>
    <Card pad="0">
      <div style={{ padding: '12px 14px 8px' }}><PanelHead title="Recent sessions with model calls" style={{ marginBottom: 0 }} /></div>
      {recent.loading && !recent.data ? <div style={{ padding: 14 }}><Skeleton height={60} /></div> : null}
      {recent.error ? <div style={{ padding: 14 }}><ErrorState what={recent.error.what} next={recent.error.next} /></div> : null}
      {recent.data && !rows.length ? <div style={{ padding: '4px 14px 14px', fontSize: 12, color: 'var(--ink-muted)' }}>No recent session made a model call.</div> : null}
      {rows.map((s) => <div key={(s.tenant || '') + '\u0000' + s.session_id} role="button" tabIndex={0}
        onClick={() => open(s.session_id, s.tenant || '')} onKeyDown={(e) => { if (e.key === 'Enter') open(s.session_id, s.tenant || ''); }}
        style={{ display: 'flex', gap: 10, alignItems: 'baseline', padding: '7px 14px', borderTop: '1px solid var(--hairline)', cursor: 'pointer', fontSize: 12 }}>
        <Mono style={{ flex: 1, minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{s.session_id}</Mono>
        <Mono color="var(--ink-muted)" style={{ whiteSpace: 'nowrap' }}>{fmtNum(s.llm_calls)} calls</Mono>
        <span className="tz-hide-narrow"><Mono size={11} color="var(--ink-faint)" style={{ whiteSpace: 'nowrap' }}>{fmtTimeNs(s.last_end_ns)}</Mono></span>
      </div>)}
    </Card>
    <EmptyState message={<span>The same reads, from a shell:</span>}
      command={`curl -s ${publicBase() || 'http://localhost:8080'}/v1/cache/<run>\ncurl -s ${publicBase() || 'http://localhost:8080'}/v1/xray/<trace_id>/<span_id>`} />
  </div>;
}
