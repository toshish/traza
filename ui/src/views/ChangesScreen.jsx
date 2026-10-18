import React from 'react';
import { api } from '../lib/api.js';
import { navigate, useRead } from '../lib/route.js';
import { fmtNum, fmtTimeNs } from '../lib/format.js';
import { routeTenant, tenantParams } from '../lib/runs.js';
import {
  CHANGE_VERDICT, DELTA_METRICS, DIMENSION_LABEL, WINDOWS, changeValue, deltaDirection, fmtP, fmtPct,
  timelinePosition, windowFrom, windowSince,
} from '../lib/insight.js';
import { Card, Chip, EmptyState, ErrorState, LoadingBar, Mono, PanelHead, RangeBar, Skeleton } from '../components/primitives/Chrome.jsx';
import { Tag } from '../components/primitives/Tag.jsx';
import { Input } from '../components/primitives/Input.jsx';
import { Caveat, DiffPair, Kv, OutcomeTag, Tooltip } from '../components/insight/Bits.jsx';

// Changes: "it worked last week", answered from evidence. Each agent's runs
// as a timeline of change points — a prompt edit, a deploy, a model the
// provider swapped under its alias, a shift in what the agent does — each
// with the runs on either side, how error and success rates, cost, steps
// and duration moved (with p-values), and the server's verdict.
// Regressions first, exactly as the server ranks them.

export function ChangesScreen({ go, params, identity }) {
  const windowId = windowFrom(params);
  const since = windowSince(windowId);
  const agentFilter = params.get('agent') || '';
  const environment = params.get('environment') || '';
  const run = params.get('run') || '';
  const tenant = routeTenant(params);
  const operator = Boolean(identity && identity.operator);
  const [envDraft, setEnvDraft] = React.useState(environment);
  const [runDraft, setRunDraft] = React.useState(run);

  const set = (changes, replace = true) => {
    const next = new URLSearchParams(params);
    for (const [key, value] of Object.entries(changes)) {
      if (value == null || value === '') next.delete(key); else next.set(key, String(value));
    }
    navigate(['changes'], Object.fromEntries(next), { replace });
  };

  const answer = useRead((signal) => api.changes({
    since, environment: environment || undefined, run: run || undefined, ...tenantParams(tenant),
  }, signal), [since, environment, run, tenant]);

  const data = answer.data;
  const agents = data ? data.agents : [];
  const shown = agentFilter ? agents.filter((a) => a.agent === agentFilter) : agents;
  const regressions = agents.reduce((n, a) => n + a.changes.filter((c) => c.verdict === 'regression').length, 0);

  return <div style={{ display: 'grid', gap: 14, maxWidth: 1500 }}>
    <div style={{ display: 'flex', alignItems: 'center', gap: '8px 14px', flexWrap: 'wrap' }}>
      <RangeBar ranges={WINDOWS} active={windowId} onPick={(id) => set({ window: id === '30d' ? null : id })} />
      <Input size="sm" mono value={envDraft} onChange={setEnvDraft} placeholder="environment (any)" ariaLabel="Environment"
        style={{ width: 170 }} onKeyDown={(e) => { if (e.key === 'Enter') set({ environment: envDraft.trim() || null }); }} />
      <Input size="sm" mono value={runDraft} onChange={setRunDraft} placeholder="place a run on its timeline" ariaLabel="Run to place"
        style={{ width: 240 }} onKeyDown={(e) => { if (e.key === 'Enter') set({ run: runDraft.trim() || null }); }} />
      {run ? <Chip active onClick={() => { setRunDraft(''); set({ run: null }); }}>run {run} ×</Chip> : null}
      {data ? <span style={{ marginLeft: 'auto', fontSize: 12, color: 'var(--ink-faint)' }}>
        <Mono size={11} color="var(--ink-muted)">{fmtNum(data.runs_scanned)}</Mono> agent runs from <Mono size={11} color="var(--ink-muted)">{fmtNum(data.spans_scanned)}</Mono> spans
      </span> : null}
    </div>
    <LoadingBar active={answer.loading} />

    {answer.error ? <ErrorState what={answer.error.status === 404 ? `Run ${run} is not among this window's agent runs.` : answer.error.what}
      next={answer.error.status === 400 && !run ? (answer.error.next || 'A window too large to analyse in time is refused; narrow it.') : answer.error.next}
      onRetry={answer.reload} /> : null}
    {!data && answer.loading ? <Skeleton height={240} /> : null}

    {data ? <>
      {data.truncated ? <Caveat>
        The analysis was bounded{data.covered_since_ns ? <>: only runs whose first step started after <Mono size={12}>{fmtTimeNs(data.covered_since_ns)}</Mono> were read</> : ''}
        {agents.some((a) => a.unnamed) ? '; some values are shown by digest because their names did not fit the name tables' : ''}.
      </Caveat> : null}

      {data.run ? <PlacedRun placed={data.run} go={go} /> : null}

      {agents.length > 1 ? <div style={{ display: 'flex', gap: 6, flexWrap: 'wrap', alignItems: 'center' }}>
        <Chip active={!agentFilter} onClick={() => set({ agent: null })}>all agents ({agents.length})</Chip>
        {agents.map((a) => {
          const reg = a.changes.filter((c) => c.verdict === 'regression').length;
          return <Chip key={a.tenant + '\u0000' + a.agent} mono active={agentFilter === a.agent} onClick={() => set({ agent: a.agent })}>
            {a.agent}{operator && a.tenant ? ` · ${a.tenant}` : ''}
            {reg ? <span style={{ color: 'var(--error)' }}>{reg}</span> : null}
          </Chip>;
        })}
        {regressions ? <span style={{ fontSize: 12, color: 'var(--error)', marginLeft: 6 }}>{regressions} regression{regressions === 1 ? '' : 's'} in this window</span> : null}
      </div> : null}

      {!agents.length ? <EmptyState message={<span>
        No agent runs in this window{environment ? <> for environment <code>{environment}</code></> : ''}. The radar reads runs
        with at least one model, tool or agent step and compares each agent's runs by their configuration
        fingerprint — set <code>service.version</code> on your resource so a deploy is a change point.
      </span>} command="OTEL_RESOURCE_ATTRIBUTES=service.version=1.5.0" /> : null}

      {shown.map((agent) => <AgentChanges key={agent.tenant + '\u0000' + agent.agent} agent={agent} operator={operator} go={go}
        placed={data.run} sinceNs={data.since_ns} untilNs={data.until_ns} />)}
    </> : null}
  </div>;
}

/** One run placed on its agent's timeline: its fingerprint, and the
    changes it ran downstream of. */
function PlacedRun({ placed, go }) {
  const dims = Object.entries(placed.fingerprint || {}).filter(([k]) => k !== 'path');
  return <Card data-testid="placed-run">
    <PanelHead title="This run on the timeline" note={placed.agent} />
    <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap', marginBottom: 10 }}>
      <a href={'#/run/' + encodeURIComponent(placed.run)} onClick={(e) => { e.preventDefault(); go(['run', placed.run], { tenant: placed.tenant || '' }); }}
        style={{ fontFamily: 'var(--font-mono)', fontSize: 13 }}>{placed.run}</a>
      <OutcomeTag outcome={placed.outcome} />
      {placed.errored ? <Tag status="error" mono>errored</Tag> : null}
      <Mono size={11} color="var(--ink-faint)">started {fmtTimeNs(placed.start_ns)}</Mono>
    </div>
    <div className="tz-reflow-1" style={{ display: 'grid', gridTemplateColumns: 'minmax(0, 1fr) minmax(0, 1fr)', gap: 16 }}>
      <div style={{ display: 'grid', gap: 3, fontSize: 12 }}>
        {dims.map(([k, v]) => <Kv key={k} k={DIMENSION_LABEL[k] || k}>{changeValue(v, 48)}</Kv>)}
        {placed.fingerprint && placed.fingerprint.path ? <Kv k="path">{placed.fingerprint.path}</Kv> : null}
      </div>
      <div style={{ display: 'grid', gap: 6 }}>
        <div style={{ fontSize: 11, textTransform: 'uppercase', letterSpacing: 'var(--tracking-caps)', color: 'var(--ink-muted)', fontWeight: 500 }}>downstream of</div>
        {!placed.downstream_of.length ? <div style={{ fontSize: 12, color: 'var(--ink-muted)' }}>No change in the window: the run used values older than every change point.</div>
          : placed.downstream_of.map((d, i) => {
            const v = CHANGE_VERDICT[d.verdict] || CHANGE_VERDICT.neutral;
            return <div key={i} style={{ display: 'grid', gap: 2, fontSize: 12 }}>
              <span style={{ display: 'flex', gap: 6, alignItems: 'center', flexWrap: 'wrap' }}>
                <Tag status={v.tone} mono>{v.label}</Tag>
                <span>{DIMENSION_LABEL[d.dimension] || d.dimension} → <Mono>{changeValue(d.to, 40)}</Mono></span>
                <Mono size={11} color="var(--ink-faint)">{fmtTimeNs(d.at_ns)}</Mono>
              </span>
              <span style={{ color: 'var(--ink-muted)' }}>{d.explanation}</span>
            </div>;
          })}
      </div>
    </div>
  </Card>;
}

/** The id of an agent's `i`th change card. An agent is a name within its
    tenant, and the operator view lists every tenant's: two tenants' agents
    of one name are two timelines, whose markers must each find their own
    card. Each part is percent-encoded with its hyphens too, so the hyphens
    between them cannot be confused with one inside a name. */
function changeAnchor(tenant, agent, i) {
  const part = (text) => encodeURIComponent(text || '').replace(/-/g, '%2D');
  return `change-${part(tenant)}-${part(agent)}-${i}`;
}

function AgentChanges({ agent, operator, go, placed, sinceNs, untilNs }) {
  const anchor = (i) => changeAnchor(agent.tenant, agent.agent, i);
  return <section aria-label={`Changes of ${agent.agent}`} style={{ display: 'grid', gap: 10 }} data-testid="agent-changes">
    <Card>
      <div style={{ display: 'flex', alignItems: 'baseline', gap: 10, flexWrap: 'wrap', marginBottom: 6 }}>
        <h2 style={{ margin: 0, fontSize: 15, fontWeight: 600, fontFamily: 'var(--font-mono)' }}>{agent.agent}</h2>
        {operator && agent.tenant ? <Mono color="var(--ink-muted)">tenant {agent.tenant}</Mono> : null}
        <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}><Mono>{fmtNum(agent.runs)}</Mono> runs · {fmtTimeNs(agent.first_run_ns)} – {fmtTimeNs(agent.last_run_ns)}</span>
      </div>
      <div style={{ fontSize: 13, lineHeight: '20px', marginBottom: 10, textWrap: 'pretty' }}>{agent.summary}</div>
      <Timeline agent={agent} placed={placed && placed.agent === agent.agent ? placed : null} anchor={anchor} />
      <div style={{ display: 'flex', gap: 6, flexWrap: 'wrap', marginTop: 10, alignItems: 'center' }}>
        <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>now</span>
        {Object.entries(agent.current || {}).map(([k, v]) => <span key={k} title={v} style={{
          fontSize: 12, border: '1px solid var(--hairline)', borderRadius: 'var(--radius-control)', padding: '1px 6px',
        }}><span style={{ color: 'var(--ink-muted)' }}>{DIMENSION_LABEL[k] || k}</span> <Mono>{changeValue(v, 36)}</Mono></span>)}
        {agent.volatile.length ? <Tag status="warn" mono title="Too varied to search: more than half the runs carry a value no other run shares, or more than four values of it, or it has over 100 change points">volatile: {agent.volatile.map((d) => DIMENSION_LABEL[d] || d).join(', ')}</Tag> : null}
      </div>
      {agent.truncated ? <Caveat>More than 40 changes; the lowest-ranked are not listed.</Caveat> : null}
    </Card>
    {agent.changes.map((change, i) => <ChangeCard key={i} id={anchor(i)} change={change} go={go} tenant={agent.tenant} />)}
  </section>;
}

const VERDICT_COLOR = { regression: 'var(--error)', improvement: 'var(--ok)', neutral: 'var(--ink-muted)', insufficient_data: 'var(--ink-faint)' };
const VERDICT_TINT = { regression: 'var(--error-tint)', improvement: 'var(--ok-tint)' };

/** Which value names a stretch when several dimensions changed at once:
    the one a person reads, before a digest. */
const LABEL_ORDER = ['service_version', 'model_returned', 'model_requested', 'sdk_version', 'params', 'behavior', 'tool_catalog', 'system_prompt'];

function namer(items) {
  const rank = (c) => { const i = LABEL_ORDER.indexOf(c.dimension); return i < 0 ? LABEL_ORDER.length : i; };
  return [...items].sort((a, b) => rank(a.change) - rank(b.change))[0].change;
}

/** The stretches between an agent's change points, each the regime the
    change at its start set up: its value, its error rate and the runs that
    rate counts, tinted by that change's verdict. The first stretch is what
    the earliest change replaced. Co-changes share a stretch, named by the
    most readable of their values.

    A change's sides stop at the neighbouring change points of any
    dimension, so a stretch is exactly the after side of the change that
    opens it (the before side of the first) — while every change point is
    listed. Past 40 the lowest-ranked are not, yet they still bound their
    neighbours' sides, so a drawn stretch may hold several sides: it is then
    given no rate. */
export function regimes(agent) {
  const columns = new Map();
  agent.changes.forEach((change, i) => {
    const key = String(change.at_ns);
    if (!columns.has(key)) columns.set(key, []);
    columns.get(key).push({ change, i });
  });
  const instants = [...columns.keys()].map(Number).sort((a, b) => a - b);
  if (!instants.length) return [];
  const bounds = [agent.first_run_ns, ...instants, agent.last_run_ns];
  const side = (change, which) => {
    const delta = !agent.truncated && change.deltas && change.deltas.error_rate;
    return delta ? { error: delta[which], n: delta[which === 'before' ? 'n_before' : 'n_after'] ?? null } : { error: null, n: null };
  };
  const out = [];
  for (let k = 0; k < bounds.length - 1; k += 1) {
    const from = bounds[k];
    const to = bounds[k + 1];
    if (k === 0) {
      const items = columns.get(String(instants[0]));
      out.push({ from, to, verdict: null, label: namer(items).from, ...side(items[0].change, 'before') });
    } else {
      const items = columns.get(String(from));
      const head = items[0].change;
      out.push({ from, to, verdict: head.verdict, label: namer(items).to, ...side(head, 'after') });
    }
  }
  return out;
}

/** The agent's runs on a time axis: the stretches between change points,
    and the change points themselves where they took over. Colour and words
    both carry the verdict. */
function Timeline({ agent, placed, anchor }) {
  const wrap = React.useRef(null);
  const [hover, setHover] = React.useState(null);
  const first = agent.first_run_ns;
  const last = agent.last_run_ns;
  // Change points at the same instant (co-changes) stack in one column.
  const columns = new Map();
  agent.changes.forEach((change, i) => {
    const key = String(change.at_ns);
    if (!columns.has(key)) columns.set(key, []);
    columns.get(key).push({ change, i });
  });
  const stretches = regimes(agent);
  const hovered = hover != null ? agent.changes[hover.i] : null;
  const markerRows = Math.max(0, ...[...columns.values()].map((c) => c.length));
  const pos = (ns) => timelinePosition(ns, first, last) * 100;
  return <div ref={wrap} style={{ position: 'relative' }} onMouseLeave={() => setHover(null)}>
    <div style={{ position: 'relative', height: 34 + markerRows * 19, margin: '0 2px' }} role="list" aria-label={`Timeline of ${agent.agent}`}>
      <div style={{ position: 'absolute', left: 0, right: 0, top: 0, height: 24, background: 'var(--bg-sunken)', borderRadius: 'var(--radius-control)' }} />
      {stretches.map((r, k) => <div key={k} data-testid="regime" title={`${changeValue(r.label, 80)}${r.error != null ? ` · error rate ${fmtPct(r.error)}${r.n != null ? ` over ${fmtNum(r.n)} finished runs` : ''}` : ''}`}
        style={{
          position: 'absolute', top: 0, height: 24, left: pos(r.from) + '%', width: Math.max(0, pos(r.to) - pos(r.from)) + '%',
          background: VERDICT_TINT[r.verdict] || 'transparent', borderLeft: k ? `2px solid ${VERDICT_COLOR[r.verdict] || 'var(--ink-faint)'}` : 'none',
          display: 'flex', alignItems: 'center', gap: 6, padding: '0 6px', overflow: 'hidden', whiteSpace: 'nowrap', boxSizing: 'border-box',
        }}>
        <Mono size={11} color="var(--ink)" style={{ overflow: 'hidden', textOverflow: 'ellipsis' }}>{changeValue(r.label, 40)}</Mono>
        {r.error != null ? <Mono size={11} color={r.verdict === 'regression' ? 'var(--error)' : 'var(--ink-muted)'}>
          err {fmtPct(r.error, 0)}{r.n != null ? <span style={{ color: 'var(--ink-faint)' }}> of {fmtNum(r.n)}</span> : null}
        </Mono> : null}
      </div>)}
      {!stretches.length ? <div style={{ position: 'absolute', left: 8, top: 4, fontSize: 12, color: 'var(--ink-faint)' }}>no change point: one regime throughout</div> : null}
      {placed ? <div title={`run ${placed.run}`} style={{
        position: 'absolute', left: `calc(${pos(placed.start_ns)}% - 5px)`, top: 7, width: 10, height: 10,
        borderRadius: '50%', border: '2px solid var(--accent)', background: 'var(--bg-raised)',
      }} /> : null}
      {[...columns.entries()].map(([key, items]) => <div key={key} style={{
        position: 'absolute', left: pos(Number(key)) + '%', top: 24, transform: 'translateX(-50%)', display: 'grid', justifyItems: 'center', gap: 3,
      }}>
        <div style={{ width: 2, height: 7, background: VERDICT_COLOR[items[0].change.verdict] }} />
        {items.map(({ change, i }) => <a key={i} href={'#' + anchor(i)} role="listitem" data-testid="change-marker"
          onClick={(e) => { e.preventDefault(); const node = document.getElementById(anchor(i)); if (node && node.scrollIntoView) node.scrollIntoView({ block: 'start', behavior: 'smooth' }); }}
          onMouseMove={(e) => { const box = wrap.current?.getBoundingClientRect(); if (box) setHover({ i, x: e.clientX - box.left, y: e.clientY - box.top, width: box.width }); }}
          style={{
            fontSize: 11, fontFamily: 'var(--font-mono)', whiteSpace: 'nowrap', textDecoration: 'none',
            color: VERDICT_COLOR[change.verdict], border: `1px solid ${VERDICT_COLOR[change.verdict]}`,
            borderStyle: change.verdict === 'insufficient_data' ? 'dashed' : 'solid',
            borderRadius: 'var(--radius-control)', padding: '0 5px', background: 'var(--bg-raised)', lineHeight: '15px',
          }}>{DIMENSION_LABEL[change.dimension] || change.dimension}</a>)}
      </div>)}
    </div>
    <div style={{ display: 'flex', justifyContent: 'space-between', fontFamily: 'var(--font-mono)', fontSize: 11, color: 'var(--ink-faint)' }}>
      <span>{fmtTimeNs(first)}</span><span>{fmtTimeNs(last)}</span>
    </div>
    {hovered ? <Tooltip x={hover.x} y={hover.y} containerWidth={hover.width} width={320}>
      <div style={{ display: 'grid', gap: 3 }}>
        <div style={{ display: 'flex', gap: 6, alignItems: 'center' }}>
          <Tag status={(CHANGE_VERDICT[hovered.verdict] || CHANGE_VERDICT.neutral).tone} mono>{(CHANGE_VERDICT[hovered.verdict] || CHANGE_VERDICT.neutral).label}</Tag>
          <span>{DIMENSION_LABEL[hovered.dimension] || hovered.dimension}</span>
        </div>
        <Kv k="from">{changeValue(hovered.from, 40)}</Kv>
        <Kv k="to">{changeValue(hovered.to, 40)}</Kv>
        <Kv k="runs before / after">{fmtNum(hovered.runs_before)} / {fmtNum(hovered.runs_after)}</Kv>
        <div style={{ color: 'var(--ink-muted)' }}>{hovered.explanation}</div>
      </div>
    </Tooltip> : null}
  </div>;
}

function ChangeCard({ change, id, go, tenant }) {
  const verdict = CHANGE_VERDICT[change.verdict] || CHANGE_VERDICT.neutral;
  const examples = change.example_runs || { before: [], after: [] };
  const runParams = { tenant: tenant || '' };
  return <Card id={id} data-testid="change" style={{ borderLeft: `3px solid ${VERDICT_COLOR[change.verdict]}` }}>
    <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap', marginBottom: 8 }}>
      <Tag status={verdict.tone} mono>{verdict.label}</Tag>
      {change.decided_by ? <Tag mono>{change.decided_by}</Tag> : null}
      <span style={{ fontSize: 14, fontWeight: 600 }}>{DIMENSION_LABEL[change.dimension] || change.dimension} changed</span>
      {change.co_changes.length ? <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>
        together with {change.co_changes.map((d) => DIMENSION_LABEL[d] || d).join(', ')}
      </span> : null}
      <span style={{ marginLeft: 'auto', display: 'flex', gap: 10, alignItems: 'baseline', fontSize: 12, color: 'var(--ink-muted)' }}>
        <Mono size={11}>{fmtTimeNs(change.at_ns)}</Mono>
        {change.effect_size ? <span title={change.decided_by === 'efficiency' ? "Cohen's d on a log scale" : "Cohen's h"}>effect <Mono color="var(--accent)">{change.effect_size.toFixed(2)}</Mono></span> : null}
      </span>
    </div>

    <div style={{ display: 'flex', gap: 8, alignItems: 'baseline', flexWrap: 'wrap', marginBottom: 6, fontSize: 12 }}>
      <Mono color="var(--ink-muted)" style={{ overflowWrap: 'anywhere' }}>{changeValue(change.from, 120)}</Mono>
      <span aria-hidden="true" style={{ color: 'var(--ink-faint)' }}>→</span>
      <Mono style={{ overflowWrap: 'anywhere' }}>{changeValue(change.to, 120)}</Mono>
    </div>
    <ChangeDetail change={change} />
    <div style={{ fontSize: 13, lineHeight: '20px', margin: '10px 0', textWrap: 'pretty' }}>{change.explanation}</div>

    <div className="tz-reflow-1" style={{ display: 'grid', gridTemplateColumns: 'minmax(0, 1.4fr) minmax(0, 1fr)', gap: 16, alignItems: 'start' }}>
      <Deltas deltas={change.deltas} />
      <div style={{ display: 'grid', gap: 10 }}>
        {change.behavior_shift ? <div style={{ fontSize: 12, color: 'var(--ink-muted)' }}>
          {change.behavior_shift.js_distance === 0 ? 'the agent took the same paths on both sides'
            : <>paths shifted by <Mono color="var(--accent)">{change.behavior_shift.js_distance.toFixed(2)}</Mono></>} (Jensen–Shannon distance, 0 same, 1 disjoint) · p <Mono>{fmtP(change.behavior_shift.p_value)}</Mono>
        </div> : null}
        <ToolMix mix={change.deltas && change.deltas.tool_mix} />
        <div style={{ display: 'grid', gap: 4, fontSize: 12 }}>
          <div style={{ color: 'var(--ink-muted)' }}><Mono>{fmtNum(change.runs_before)}</Mono> finished runs before, <Mono>{fmtNum(change.runs_after)}</Mono> after; nearest the change:</div>
          {['before', 'after'].map((side) => <div key={side} style={{ display: 'flex', gap: 6, flexWrap: 'wrap', alignItems: 'center' }}>
            <span style={{ width: 42, color: 'var(--ink-faint)' }}>{side}</span>
            {(examples[side] || []).map((r) => <a key={r} href={'#/run/' + encodeURIComponent(r)}
              onClick={(e) => { e.preventDefault(); go(['run', r], runParams); }} style={{ fontFamily: 'var(--font-mono)', fontSize: 12 }}>{r}</a>)}
          </div>)}
          {examples.before && examples.before[0] && examples.after && examples.after[0] ? <div style={{ marginTop: 4 }}>
            <Chip onClick={() => go(['diff'], { a: examples.before[0], b: examples.after[0], ...runParams })}>Diff the last run before against the first after</Chip>
          </div> : null}
        </div>
      </div>
    </div>
  </Card>;
}

/** The dimension's own evidence: the prompt edit, the tools added and
    removed, the parameters that differ, the paths that moved. */
function ChangeDetail({ change }) {
  const d = change.detail;
  if (!d) return null;
  if (d.diff) {
    return <div style={{ padding: '7px 9px', background: 'var(--bg-sunken)', borderRadius: 'var(--radius-control)', display: 'grid', gap: 3 }}>
      <DiffPair diff={{ before: d.diff.before, after: d.diff.after, offset: d.diff.offset }} context={d.diff.context} />
      {typeof d.before_chars === 'number' ? <span style={{ fontSize: 11, color: 'var(--ink-faint)' }}>prompt {d.before_chars} → {d.after_chars} characters, volatile tokens masked</span> : null}
    </div>;
  }
  if (d.added || d.removed || d.definitions_changed) {
    return <div style={{ display: 'flex', gap: 6, flexWrap: 'wrap', fontSize: 12 }}>
      {(d.added || []).map((t) => <Tag key={'+' + t} status="ok" mono>+ {t}</Tag>)}
      {(d.removed || []).map((t) => <Tag key={'-' + t} status="error" mono>− {t}</Tag>)}
      {d.definitions_changed ? <Tag mono>definitions changed</Tag> : null}
    </div>;
  }
  if (d.changed) {
    return <div style={{ display: 'grid', gap: 3, fontSize: 12 }}>
      {d.changed.map((c) => <div key={c.key}><Mono color="var(--ink-muted)">{c.key}</Mono> <Mono>{paramValue(c.before)}</Mono> → <Mono>{paramValue(c.after)}</Mono></div>)}
    </div>;
  }
  if (d.paths) {
    return <div style={{ display: 'grid', gap: 4 }} aria-label="Paths whose share moved most">
      {d.paths.map((p) => <div key={p.path} style={{ display: 'grid', gridTemplateColumns: 'minmax(0, 1fr) 160px', gap: 10, alignItems: 'center', fontSize: 12 }}>
        <Mono size={11} style={{ overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }} title={p.path}>{p.path}</Mono>
        <SharePair before={p.before_share} after={p.after_share} />
      </div>)}
    </div>;
  }
  return null;
}

/** A before and an after share as two thin bars, ink then accent. */
function SharePair({ before, after, max = 1 }) {
  const w = (v) => Math.max(0, Math.min(1, (v || 0) / (max || 1))) * 100 + '%';
  return <span style={{ display: 'grid', gap: 2 }} title={`before ${fmtPct(before, 0)}, after ${fmtPct(after, 0)}`}>
    <span style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
      <span style={{ flex: 1, height: 5, background: 'var(--bg-sunken)', borderRadius: 1.5, position: 'relative' }}>
        <span style={{ position: 'absolute', left: 0, top: 0, bottom: 0, width: w(before), background: 'var(--series-3)', borderRadius: 1.5 }} />
      </span>
      <Mono size={11} color="var(--ink-muted)" style={{ width: 34, textAlign: 'right' }}>{fmtPct(before, 0)}</Mono>
    </span>
    <span style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
      <span style={{ flex: 1, height: 5, background: 'var(--bg-sunken)', borderRadius: 1.5, position: 'relative' }}>
        <span style={{ position: 'absolute', left: 0, top: 0, bottom: 0, width: w(after), background: 'var(--accent)', borderRadius: 1.5 }} />
      </span>
      <Mono size={11} color="var(--accent)" style={{ width: 34, textAlign: 'right' }}>{fmtPct(after, 0)}</Mono>
    </span>
  </span>;
}

/** A sampling parameter's value as the server types it: JSON, so the
    number `1024` and the string `"1024"` — two configurations — read apart,
    and an absent key is `∅`. */
export function paramValue(value) {
  if (value === null || value === undefined) return '∅';
  return JSON.stringify(value);
}

/** Observations a side a metric's own test needs before the server lets it
    take part in a verdict (runs that ended, for success rate; priced runs,
    for cost). Below it the row is still shown, never marked. */
export const MIN_SIDE_OBSERVATIONS = 10;

/** The deltas as a table: before, after, the change, its p-value and how
    many runs each side counted. A row is marked by direction only when its
    raw p is under 5% and its own test had ten observations a side, as the
    server requires before a metric counts; the verdict itself is the
    server's, Holm-corrected. */
function Deltas({ deltas }) {
  if (!deltas) return null;
  return <div className="tz-scroll-x"><div role="table" aria-label="Deltas" style={{ display: 'grid', gridTemplateColumns: 'minmax(96px, 1.2fr) minmax(64px, 1fr) minmax(88px, 1fr) 56px 56px', fontSize: 12, alignItems: 'center', minWidth: 360 }}>
    {['metric', 'before', 'after', 'p', 'runs'].map((h, i) => <div key={h} role="columnheader" style={{
      color: 'var(--ink-muted)', padding: '3px 6px', borderBottom: '1px solid var(--hairline)', textAlign: i ? 'right' : 'left',
    }}>{h}</div>)}
    {DELTA_METRICS.map((metric) => {
      const delta = deltas[metric.key];
      if (!delta) return null;
      const direction = deltaDirection(metric, delta);
      const counted = (delta.n_before ?? 0) >= MIN_SIDE_OBSERVATIONS && (delta.n_after ?? 0) >= MIN_SIDE_OBSERVATIONS;
      const significant = counted && typeof delta.p_value === 'number' && delta.p_value < 0.05;
      const color = significant && direction === 'worse' ? 'var(--error)' : significant && direction === 'better' ? 'var(--ok)' : 'var(--ink)';
      return <React.Fragment key={metric.key}>
        <div role="cell" style={{ padding: '4px 6px', color: 'var(--ink-muted)' }}>{metric.label}</div>
        <div role="cell" style={{ padding: '4px 6px', textAlign: 'right' }}><Mono color="var(--ink-muted)">{delta.before == null ? '—' : metric.fmt(delta.before)}</Mono></div>
        <div role="cell" style={{ padding: '4px 6px', textAlign: 'right' }}>
          <Mono color={color} style={{ fontWeight: significant ? 600 : 400 }}>{delta.after == null ? '—' : metric.fmt(delta.after)}</Mono>
          {typeof delta.ratio === 'number' ? <Mono size={11} color="var(--ink-faint)"> {delta.ratio.toFixed(2)}×</Mono> : null}
        </div>
        <div role="cell" style={{ padding: '4px 6px', textAlign: 'right' }}><Mono color={significant ? 'var(--ink)' : 'var(--ink-faint)'}>{fmtP(delta.p_value)}</Mono></div>
        <div role="cell" style={{ padding: '4px 6px', textAlign: 'right' }} title={counted ? undefined : `Fewer than ${MIN_SIDE_OBSERVATIONS} observations on a side: shown, but not part of the verdict`}>
          <Mono size={11} color={counted ? 'var(--ink-faint)' : 'var(--warn)'}>{fmtNum(delta.n_before)}/{fmtNum(delta.n_after)}</Mono>
        </div>
      </React.Fragment>;
    })}
  </div></div>;
}

function ToolMix({ mix }) {
  if (!mix || !mix.length) return null;
  const max = Math.max(...mix.map((t) => Math.max(t.before_share, t.after_share)), 0.01);
  return <div style={{ display: 'grid', gap: 4 }}>
    <div style={{ fontSize: 11, textTransform: 'uppercase', letterSpacing: 'var(--tracking-caps)', color: 'var(--ink-muted)', fontWeight: 500 }}>tool mix (shown, not tested)</div>
    {mix.slice(0, 6).map((t) => <div key={t.tool} style={{ display: 'grid', gridTemplateColumns: 'minmax(0, 1fr) 150px', gap: 8, alignItems: 'center', fontSize: 12 }}>
      <Mono size={12} style={{ overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{t.tool}</Mono>
      <SharePair before={t.before_share} after={t.after_share} max={max} />
    </div>)}
  </div>;
}
