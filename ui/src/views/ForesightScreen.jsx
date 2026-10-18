import React from 'react';
import { api } from '../lib/api.js';
import { navigate, useRead } from '../lib/route.js';
import { fmtNum, fmtTimeNs, fmtDurationNs } from '../lib/format.js';
import { fmtMoney, routeTenant, tenantParams } from '../lib/runs.js';
import {
  FORESIGHT_VERDICT, WINDOWS, bestSkill, fmtCi, fmtP, fmtPct, nodeAt, nodeMeans, pathKey, pathToTql,
  runOnMap, skillReading, windowFrom, windowSince,
} from '../lib/insight.js';
import { Card, Chip, EmptyState, ErrorState, LoadingBar, Mono, PanelHead, RangeBar, Skeleton } from '../components/primitives/Chrome.jsx';
import { Tag } from '../components/primitives/Tag.jsx';
import { CiBar, Caveat, Kv, OutcomeTag, PathTokens, Tile, TileRow, Token } from '../components/insight/Bits.jsx';
import { FateLegend, FateMap } from '../components/insight/FateMap.jsx';
import { NextOptions } from '../components/insight/NextOptions.jsx';

// Foresight: an agent's Fate Map. Every path its completed runs took, how
// often each ended well, where the failing paths part from the rest — and,
// opened from a run, where that run is on the map and what earlier runs did
// next from there. Counting, carefully: every figure is the server's
// arithmetic over stored spans, and the map scores its own predictions so a
// reader knows how far to trust a verdict.

const SUPPORT_CHOICES = [1, 3, 5, 10, 30];
const DEPTH_CHOICES = [6, 12, 24];

/** The cohort a route names, as a key that tells task sub-cohorts and
    tenants apart. */
function cohortKey(c) {
  return JSON.stringify([c.tenant || '', c.agent, c.task || null]);
}

export function ForesightScreen({ go, params, identity }) {
  const windowId = windowFrom(params);
  const since = windowSince(windowId);
  const agent = params.get('agent') || '';
  const task = params.get('task') || '';
  const run = params.get('run') || '';
  const tenant = routeTenant(params);
  const scope = tenantParams(tenant);
  const minSupport = SUPPORT_CHOICES.includes(Number(params.get('min'))) ? Number(params.get('min')) : 3;
  const depth = DEPTH_CHOICES.includes(Number(params.get('depth'))) ? Number(params.get('depth')) : 12;
  const selectedKey = params.has('node') ? params.get('node') : null;
  const operator = Boolean(identity && identity.operator);

  const set = (changes, { replace = true } = {}) => {
    const next = new URLSearchParams(params);
    for (const [key, value] of Object.entries(changes)) {
      if (value === undefined || value === null || value === false) next.delete(key);
      else next.set(key, String(value));
    }
    navigate(['foresight'], Object.fromEntries(next), { replace });
  };

  const cohorts = useRead((signal) => api.fateCohorts({ since }, signal), [since]);
  const foresight = useRead((signal) => api.fateRun(run, scope, signal), [run, tenant], { skip: !run });
  // The run names its agent; a map opened from a run follows it.
  const runAgent = foresight.data && foresight.data.agent;
  const shownAgent = agent || runAgent || (cohorts.data && cohorts.data.cohorts && cohorts.data.cohorts[0] && cohorts.data.cohorts[0].agent) || '';
  // The tenant travels with the cohort: named in the route, else the listed
  // cohort's own, else the run's. Run and diff links carry it, so a
  // default-tenant run is not read across every tenant.
  const listed = cohorts.data && cohorts.data.cohorts ? cohorts.data.cohorts.find((c) => c.agent === shownAgent && (c.task || '') === task) : null;
  const shownTenant = tenant !== undefined ? tenant
    : runAgent && foresight.data.agent === shownAgent && typeof foresight.data.tenant === 'string' ? foresight.data.tenant
      : listed ? listed.tenant : undefined;
  // The map waits for what decides its cohort and tenant, so it is asked
  // for once rather than once per answer that refines them.
  const ready = Boolean((cohorts.data || cohorts.error) && (!run || foresight.data || foresight.error));
  const map = useRead((signal) => api.fateMap({
    agent: shownAgent, task: task || undefined, since, depth, min_support: minSupport,
    ...tenantParams(shownTenant),
  }, signal), [shownAgent, task, since, depth, minSupport, shownTenant], { skip: !shownAgent || !ready });

  const [collapsed, setCollapsed] = React.useState(() => new Set());
  React.useEffect(() => { setCollapsed(new Set()); }, [shownAgent, task, since, depth, minSupport]);
  const toggle = (key) => setCollapsed((old) => {
    const next = new Set(old);
    if (next.has(key)) next.delete(key); else next.add(key);
    return next;
  });

  const data = map.data;
  // The run is lit only on the map it was judged against: its agent's whole
  // cohort, in its tenant. A task's map, or another tenant's agent of the
  // same name, would place it on paths it was not compared with.
  const sameCohort = Boolean(foresight.data && data && foresight.data.agent === data.agent && !task
    && (foresight.data.tenant || '') === (data.tenant || ''));
  const mismatch = !foresight.data || !data || sameCohort ? null
    : foresight.data.agent !== data.agent ? `This run belongs to ${foresight.data.agent}; the map shown is ${data.agent}. Pick its cohort to see it on the map.`
      : task ? `The map shown is one task of ${data.agent}; foresight judged this run against the agent's whole cohort, so it is not lit here.`
        : `The map shown is tenant ${data.tenant || '(default)'}'s ${data.agent}; this run is tenant ${foresight.data.tenant || '(default)'}'s.`;
  const highlight = React.useMemo(
    () => (sameCohort ? runOnMap(data.root, foresight.data) : null),
    [sameCohort, data, foresight.data],
  );
  const activeKey = selectedKey != null ? selectedKey : highlight ? highlight.atKey : '';
  const activeNode = data ? nodeAt(data.root, activeKey ? activeKey.split('\u0001') : []) : null;

  const pickCohort = (c) => {
    const changes = { agent: c.agent, task: c.task || null, node: null, tenant: operator ? (c.tenant || '') : null };
    if (run && runAgent && runAgent !== c.agent) changes.run = null;
    set(changes, { replace: false });
  };

  const list = cohorts.data ? cohorts.data.cohorts || [] : [];
  const shownKey = cohortKey({ tenant: shownTenant || '', agent: shownAgent, task: task || null });

  return <div style={{ display: 'grid', gap: 14, maxWidth: 1800 }}>
    <CohortPicker cohorts={cohorts} list={list} shownKey={shownKey} operator={operator} onPick={pickCohort} windowId={windowId} />

    <div style={{ display: 'flex', alignItems: 'center', gap: '8px 16px', flexWrap: 'wrap' }}>
      <RangeBar ranges={WINDOWS} active={windowId} onPick={(id) => set({ window: id === '30d' ? null : id, node: null })} />
      <span style={{ display: 'inline-flex', alignItems: 'center', gap: 6 }}>
        <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>min support</span>
        {SUPPORT_CHOICES.map((n) => <Chip key={n} mono active={minSupport === n} onClick={() => set({ min: n === 3 ? null : n })}>{n}</Chip>)}
      </span>
      <span style={{ display: 'inline-flex', alignItems: 'center', gap: 6 }}>
        <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>depth</span>
        {DEPTH_CHOICES.map((n) => <Chip key={n} mono active={depth === n} onClick={() => set({ depth: n === 12 ? null : n })}>{n}</Chip>)}
      </span>
      {data ? <span style={{ marginLeft: 'auto', fontSize: 12, color: 'var(--ink-faint)' }}>
        built <Mono color="var(--ink-muted)" size={11}>{fmtTimeNs(data.built_at_ns)}</Mono> from <Mono color="var(--ink-muted)" size={11}>{fmtNum(data.bounds.spans_read)}</Mono> spans
      </span> : null}
    </div>
    <LoadingBar active={map.loading || cohorts.loading || foresight.loading} />

    {run ? <RunForesightBanner foresight={foresight} run={run} tenant={tenant} go={go}
      mismatch={mismatch} windowId={windowId} /> : null}

    <MapBody map={map} data={data} shownAgent={shownAgent} task={task} windowId={windowId} cohorts={list}
      highlight={highlight} activeKey={activeKey} activeNode={activeNode} collapsed={collapsed} onToggle={toggle}
      onSelect={(key) => set({ node: key })} go={go} tenant={shownTenant} />
  </div>;
}

/** The cohorts as cards: each agent's completed runs and base rate, its
    task sub-cohorts beside it. */
function CohortPicker({ cohorts, list, shownKey, operator, onPick, windowId }) {
  if (cohorts.error) return <ErrorState what={cohorts.error.what} next={cohorts.error.next} onRetry={cohorts.reload} />;
  if (cohorts.loading && !cohorts.data) return <Skeleton height={64} />;
  if (!list.length) {
    return <EmptyState message={<span>
      No agent has completed runs in this window ({windowId}). A cohort is the runs of one agent — the
      first step's <code>gen_ai.agent.name</code>, else the service — whose steps (model, tool and agent
      spans) ended in a known outcome. Widen the window, or load the behaviour corpus to see a map:
    </span>} command={'seed --data-dir ./data --behaviours\ntraza-server --data-dir ./data'} />;
  }
  // One row that scrolls sideways: on a phone, five stacked cards would push
  // the map a screen down.
  return <div role="listbox" aria-label="Cohorts" className="tz-scroll-x" style={{ display: 'flex', gap: 8, paddingBottom: 2 }}>
    {list.map((c) => {
      const key = cohortKey(c);
      const active = key === shownKey;
      return <button key={key} type="button" role="option" aria-selected={active} onClick={() => onPick(c)}
        className="tz-plain-button" data-testid="cohort"
        style={{
          display: 'grid', gap: 4, minWidth: 180, flex: 'none', padding: '8px 12px', cursor: 'pointer',
          border: `1px solid ${active ? 'var(--accent)' : 'var(--hairline)'}`, borderRadius: 'var(--radius-card)',
          background: active ? 'var(--accent-tint)' : 'var(--bg-raised)', marginLeft: c.task ? 0 : 0,
        }}>
        <span style={{ display: 'flex', alignItems: 'baseline', gap: 6, minWidth: 0 }}>
          <span style={{ fontFamily: 'var(--font-mono)', fontSize: 12, fontWeight: 500, color: 'var(--ink)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{c.agent}</span>
          {c.task ? <span title={'task ' + c.task} style={{ fontFamily: 'var(--font-mono)', fontSize: 11, color: 'var(--ink-faint)' }}>task {c.task.slice(0, 8)}</span> : null}
          {operator && c.tenant ? <span style={{ fontFamily: 'var(--font-mono)', fontSize: 11, color: 'var(--ink-faint)' }}>{c.tenant}</span> : null}
        </span>
        <span style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
          <span style={{ position: 'relative', width: 70, height: 6, background: 'var(--bg-sunken)', borderRadius: 1.5 }}>
            <span style={{ position: 'absolute', left: 0, top: 0, bottom: 0, width: ((c.base_rate || 0) * 100) + '%', background: 'var(--accent)', borderRadius: 1.5 }} />
          </span>
          <span style={{ fontFamily: 'var(--font-mono)', fontSize: 12, color: 'var(--accent)', fontVariantNumeric: 'tabular-nums' }}>{fmtPct(c.base_rate, 0)}</span>
          <span style={{ fontFamily: 'var(--font-mono)', fontSize: 11, color: 'var(--ink-muted)', fontVariantNumeric: 'tabular-nums' }}>{fmtNum(c.runs)} runs</span>
          {c.unknown ? <span title="still active, or an unrecognised outcome: excluded from every rate" style={{ fontFamily: 'var(--font-mono)', fontSize: 11, color: 'var(--ink-faint)' }}>+{fmtNum(c.unknown)} live</span> : null}
        </span>
      </button>;
    })}
  </div>;
}

function MapBody({ map, data, shownAgent, task, windowId, highlight, activeKey, activeNode, collapsed, onToggle, onSelect, go, tenant }) {
  if (!shownAgent) return null;
  if (map.error) {
    return <ErrorState what={map.error.status === 404 ? `No runs of ${shownAgent} in this window.` : map.error.what}
      next={map.error.status === 404 ? 'Widen the window, or pick another cohort.'
        : map.error.status === 400 ? (map.error.next || 'A window too large to read in time is refused; narrow it.') : map.error.next}
      onRetry={map.reload} />;
  }
  if (!data) return <Skeleton height={380} />;
  const skill = bestSkill(data.calibration);
  const caveats = boundsCaveats(data);
  return <>
    <TileRow columns={7}>
      <Tile label="completed runs" value={fmtNum(data.runs)} testId="tile-runs" />
      <Tile label="base rate" value={fmtPct(data.base_rate)} />
      <Tile label="live or unknown" value={fmtNum(data.unknown)} tone="var(--ink)" title="Runs whose outcome cannot be told yet: excluded from every rate, counted here" />
      <Tile label="nodes shown" value={`${fmtNum(data.nodes_shown)} / ${fmtNum(data.nodes_total)}`} tone="var(--ink)" />
      <Tile label="fatal forks" value={fmtNum(data.fatal_forks.length)} tone={data.fatal_forks.length ? 'var(--error)' : 'var(--ink)'} />
      <Tile label="hotspots" value={fmtNum(data.hotspots.length)} tone="var(--ink)" />
      <Tile label="best skill" value={skill ? fmtPct(skill.skill, 0) : '—'} note={skill ? `depth ${skill.depth}` : ''}
        title="How much of the base rate's prediction error the map removes, scored on runs it did not see" />
    </TileRow>
    {caveats.length ? <div style={{ display: 'grid', gap: 3 }}>{caveats.map((c) => <Caveat key={c}>{c}</Caveat>)}</div> : null}

    <Card pad="12px 14px 6px">
      <PanelHead title={`Fate Map · ${data.agent}${task ? ' · task ' + task.slice(0, 8) : ''}`}
        note="hover a node to read it, click to open it, double-click to fold its branches" />
      <FateLegend base={data.base_rate} />
      {!(data.root.children || []).length && data.root.omitted_children ? <div style={{ marginTop: 8 }}>
        <Caveat testId="all-pruned">Every branch from the start has fewer than {data.min_support} runs, so none is drawn ({data.root.omitted_children} branch{data.root.omitted_children === 1 ? '' : 'es'}, {data.root.omitted_visits} runs). Lower min support to see them.</Caveat>
      </div> : null}
      <div style={{ marginTop: 8 }}>
        <FateMap map={data} highlight={highlight} selected={activeKey} onSelect={onSelect} collapsed={collapsed} onToggle={onToggle} />
      </div>
    </Card>

    <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(320px, 1fr))', gap: 14, alignItems: 'start' }}>
      <NodePanel node={activeNode} nodeKey={activeKey} base={data.base_rate} go={go} tenant={tenant} windowId={windowId}
        folded={collapsed.has(activeKey)} onToggle={onToggle} />
      <ForksCard forks={data.fatal_forks} base={data.base_rate} onSelect={onSelect} />
      <CalibrationCard calibration={data.calibration} />
      <HotspotsCard hotspots={data.hotspots} onSelect={onSelect} />
    </div>
  </>;
}

/** What bounded the build, in words — pruning is always reported. */
export function boundsCaveats(data) {
  const out = [];
  const b = data.bounds || {};
  if (b.window_narrowed) out.push(`The build narrowed its window to fit its run bound: it starts ${fmtTimeNs(data.window.since_ns)}.`);
  if (data.nodes_capped || b.node_cap) out.push('The node bound cut this tree: deep, rare paths are missing, and a run on one reads as insufficient history.');
  if (b.runs_dropped) out.push(`${fmtNum(b.runs_dropped)} of the oldest completed runs were dropped by the per-cohort bound.`);
  if (b.runs_partial) out.push(`${fmtNum(b.runs_partial)} runs were read in part (over 192 steps); their paths end in …`);
  if (b.signature_cap) out.push('The distinct-signature bound was reached; some paths end early.');
  if (b.cohorts_dropped) out.push(`${fmtNum(b.cohorts_dropped)} smaller cohorts were dropped by the cohort bound.`);
  if (b.spans_skipped) out.push(`${fmtNum(b.spans_skipped)} spans were skipped by a build bound.`);
  return out;
}

/** The selected node: its figures, its example runs, and what to do next
    with them. */
function NodePanel({ node, nodeKey, base, go, tenant, windowId, folded, onToggle }) {
  if (!node) return <Card><PanelHead title="Node" /><div style={{ fontSize: 12, color: 'var(--ink-muted)' }}>Click a node of the map.</div></Card>;
  const path = nodeKey ? nodeKey.split('\u0001') : [];
  const means = nodeMeans(node);
  const examples = node.example_runs || [];
  const success = examples.find((e) => e.outcome === 'success');
  const failure = examples.find((e) => e.outcome === 'failure') || examples.find((e) => e.outcome === 'abandoned');
  const tql = pathToTql(path);
  const runParams = tenant !== undefined ? { tenant } : undefined;
  return <Card data-testid="node-panel">
    <PanelHead title={path.length ? 'Node' : 'Start'} note={path.length ? `depth ${node.depth}` : 'every run of the cohort'} />
    <div style={{ display: 'grid', gap: 10 }}>
      {path.length ? <PathTokens path={path} accentIndex={path.length - 1} /> : null}
      <div style={{ display: 'flex', gap: 6, flexWrap: 'wrap' }}>
        {node.fatal_fork ? <Tag status="error" mono>fatal fork</Tag> : null}
        {node.hotspot ? <Tag status="warn" mono>hotspot</Tag> : null}
      </div>
      <CiBar rate={node.success_rate} ci={node.ci95} base={base} width={170} />
      <div style={{ display: 'grid', gap: 3, fontSize: 12 }}>
        <Kv k="runs through here">{fmtNum(node.visits)}</Kv>
        <Kv k="success / failure / abandoned">{fmtNum(node.success)} / {fmtNum(node.failure)} / {fmtNum(node.abandoned)}</Kv>
        <Kv k="95% Wilson interval">{fmtCi(node.ci95)}</Kv>
        <Kv k="p(failure | here)">{fmtPct(node.p_fail_given_here)}</Kv>
        {typeof node.lift === 'number' ? <Kv k="lift vs the agent">{node.lift.toFixed(2)}×</Kv> : null}
        {means.cost != null ? <Kv k="mean cost per run">${fmtMoney(means.cost)}</Kv> : null}
        {means.durationNs != null ? <Kv k="mean run duration">{fmtDurationNs(means.durationNs)}</Kv> : null}
        <Kv k="ended here">{fmtNum(node.ended || 0)}</Kv>
        <Kv k="entropy of the next step">{(node.entropy ?? 0).toFixed(2)} bits</Kv>
        {node.fork ? <>
          <Kv k="siblings' success">{fmtPct(node.fork.siblings_success_rate)} of {fmtNum(node.fork.siblings_visits)}</Kv>
          <Kv k="gap · Fisher p">{(node.fork.gap * 100).toFixed(1)} pts · {fmtP(node.fork.p_value)}</Kv>
        </> : null}
        {node.omitted_children ? <Kv k="branches not drawn">{fmtNum(node.omitted_children)} ({fmtNum(node.omitted_visits)} runs)</Kv> : null}
        {node.cut ? <Kv k="runs past the node bound">{fmtNum(node.cut)}</Kv> : null}
      </div>
      {examples.length ? <div style={{ display: 'grid', gap: 5 }}>
        <div style={{ fontSize: 11, textTransform: 'uppercase', letterSpacing: 'var(--tracking-caps)', color: 'var(--ink-muted)', fontWeight: 500 }}>example runs</div>
        {examples.map((e) => <div key={e.run} style={{ display: 'flex', alignItems: 'center', gap: 8, minWidth: 0 }}>
          <OutcomeTag outcome={e.outcome} />
          <a href={'#/run/' + encodeURIComponent(e.run)} onClick={(ev) => { ev.preventDefault(); go(['run', e.run], runParams); }}
            style={{ fontFamily: 'var(--font-mono)', fontSize: 12, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{e.run}</a>
        </div>)}
      </div> : null}
      <div style={{ display: 'flex', gap: 6, flexWrap: 'wrap' }}>
        {success && failure ? <Chip onClick={() => go(['diff'], { a: success.run, b: failure.run, ...(runParams || {}) })}
          title="Align a success and a failure from this point and name where they parted">Diff success vs {failure.outcome}</Chip> : null}
        {tql ? <Chip onClick={() => go(['behaviors'], { q: tql, ...(windowId !== '30d' ? { window: windowId } : {}) })}
          title={'Search for runs that took this path: ' + tql}>Find these runs</Chip> : null}
        {(node.children || []).length ? <Chip onClick={() => onToggle(nodeKey)}>{folded ? 'Unfold branches' : 'Fold branches'}</Chip> : null}
      </div>
    </div>
  </Card>;
}

function ForksCard({ forks, base, onSelect }) {
  return <Card>
    <PanelHead title="Fatal forks" note="≥ 25 points below the siblings, Fisher p < 0.05" />
    {!forks.length ? <div style={{ fontSize: 12, color: 'var(--ink-muted)', lineHeight: '18px' }}>
      No branch does significantly worse than its siblings. Three unlucky runs are never called a fork: the
      gap must also pass a Fisher exact test.
    </div> : <div style={{ display: 'grid', gap: 10 }}>
      {forks.map((f) => <button key={pathKey(f.path)} type="button" className="tz-plain-button" data-testid="fatal-fork"
        onClick={() => onSelect(pathKey(f.path))}
        style={{ display: 'grid', gap: 5, cursor: 'pointer', paddingBottom: 9, borderBottom: '1px solid var(--hairline)' }}>
        <PathTokens path={f.path} accentIndex={f.path.length - 1} />
        <span style={{ display: 'flex', gap: 10, alignItems: 'center', flexWrap: 'wrap', fontSize: 12, color: 'var(--ink-muted)' }}>
          <CiBar rate={f.success_rate} base={base} width={90} label={false} />
          <Mono color="var(--error)">{fmtPct(f.success_rate)}</Mono>
          <span>of <Mono>{fmtNum(f.visits)}</Mono> vs siblings <Mono>{fmtPct(f.fork.siblings_success_rate)}</Mono> of <Mono>{fmtNum(f.fork.siblings_visits)}</Mono></span>
          <span>p <Mono>{fmtP(f.fork.p_value)}</Mono></span>
        </span>
      </button>)}
    </div>}
    <div style={{ marginTop: 10, fontSize: 11, color: 'var(--ink-faint)', lineHeight: '16px' }}>
      Correlation, not cause: a fork says where runs that went that way ended worse. Diff a success against a
      failure from the same node to see why.
    </div>
  </Card>;
}

function HotspotsCard({ hotspots, onSelect }) {
  return <Card>
    <PanelHead title="Hotspots" note="where the agent cannot make up its mind" />
    {!hotspots.length ? <div style={{ fontSize: 12, color: 'var(--ink-muted)' }}>No node has a next step with a bit or more of entropy.</div>
      : <div style={{ display: 'grid', gap: 10 }}>
        {hotspots.map((h) => <button key={pathKey(h.path)} type="button" className="tz-plain-button" onClick={() => onSelect(pathKey(h.path))}
          style={{ display: 'grid', gap: 5, cursor: 'pointer', paddingBottom: 9, borderBottom: '1px solid var(--hairline)' }}>
          <PathTokens path={h.path} accentIndex={h.path.length - 1} />
          <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>
            <Mono color="var(--accent)">{h.entropy.toFixed(2)}</Mono> bits over <Mono>{fmtNum(h.visits)}</Mono> runs; next:
          </span>
          <span style={{ display: 'flex', gap: 6, flexWrap: 'wrap' }}>{h.next.map((t) => <span key={t} style={{ border: '1px solid var(--hairline)', borderRadius: 'var(--radius-control)', padding: '0 4px' }}><Token token={t} /></span>)}</span>
        </button>)}
      </div>}
  </Card>;
}

/** The map's own score: each run predicted from the half of the cohort it
    is not in, at several depths, against the base rate alone. */
export function CalibrationCard({ calibration }) {
  const depths = (calibration && calibration.depths) || [];
  const reliability = (calibration && calibration.reliability) || [];
  const best = bestSkill(calibration);
  return <Card data-testid="calibration">
    <PanelHead title="How good is this map" note="two-fold cross-validation" />
    {!depths.length ? <div style={{ fontSize: 12, color: 'var(--ink-muted)' }}>Too few runs to score.</div> : <>
      <div style={{ fontSize: 12, color: 'var(--ink-muted)', lineHeight: '18px', marginBottom: 8 }}>
        At its best the map is <strong style={{ color: 'var(--ink)', fontWeight: 500 }}>{skillReading(best && best.skill)}</strong>
        {best ? <> (skill <Mono color="var(--accent)">{fmtPct(best.skill, 0)}</Mono> at depth {best.depth})</> : null}.
        {best && best.skill < 0.05 ? ' Do not act on its verdicts: outcome barely depends on path here.' : ' Read skill before trusting a verdict.'}
      </div>
      <div role="table" aria-label="Calibration by depth" style={{ display: 'grid', gridTemplateColumns: '44px 56px 64px 64px minmax(60px, 1fr)', fontSize: 12 }}>
        {['depth', 'runs', 'Brier', 'base', 'skill'].map((h) => <div key={h} role="columnheader" style={{ color: 'var(--ink-muted)', padding: '2px 0', borderBottom: '1px solid var(--hairline)' }}>{h}</div>)}
        {depths.map((row) => <React.Fragment key={row.depth}>
          <div role="cell" style={{ fontFamily: 'var(--font-mono)', padding: '3px 0' }}>{row.depth}</div>
          <div role="cell" style={{ fontFamily: 'var(--font-mono)', padding: '3px 0' }} title={`${row.covered} of ${row.runs} reached this depth`}>{fmtNum(row.covered)}</div>
          <div role="cell" style={{ fontFamily: 'var(--font-mono)', padding: '3px 0' }}>{row.brier.toFixed(3)}</div>
          <div role="cell" style={{ fontFamily: 'var(--font-mono)', padding: '3px 0', color: 'var(--ink-muted)' }}>{row.baseline_brier.toFixed(3)}</div>
          <div role="cell" style={{ display: 'flex', alignItems: 'center', gap: 6, padding: '3px 0' }}>
            <span style={{ position: 'relative', flex: 1, height: 6, background: 'var(--bg-sunken)', borderRadius: 1.5, maxWidth: 90 }}>
              <span style={{ position: 'absolute', left: 0, top: 0, bottom: 0, width: Math.max(0, Math.min(1, row.skill)) * 100 + '%', background: 'var(--accent)', borderRadius: 1.5 }} />
            </span>
            <span style={{ fontFamily: 'var(--font-mono)', color: row.skill < 0 ? 'var(--error)' : 'var(--accent)' }}>{fmtPct(row.skill, 0)}</span>
          </div>
        </React.Fragment>)}
      </div>
      {reliability.length ? <Reliability bins={reliability} /> : null}
    </>}
  </Card>;
}

/** Depth-3 predictions binned in tenths: predicted against what happened.
    On the diagonal, the map means what it says. */
function Reliability({ bins }) {
  const size = 132;
  const max = Math.max(...bins.map((b) => b.runs), 1);
  return <div style={{ display: 'flex', gap: 12, alignItems: 'center', marginTop: 12 }}>
    <svg width={size + 8} height={size + 8} role="img" aria-label="Reliability: predicted against observed success, depth 3" style={{ flex: 'none' }}>
      <rect x="4" y="4" width={size} height={size} fill="none" stroke="var(--hairline)" />
      <line x1="4" y1={size + 4} x2={size + 4} y2="4" stroke="var(--ink-faint)" strokeWidth="1" />
      {bins.map((b) => <circle key={b.low} cx={4 + b.predicted * size} cy={4 + (1 - b.observed) * size}
        r={3 + 5 * Math.sqrt(b.runs / max)} fill="var(--accent)" fillOpacity="0.75" stroke="var(--bg-raised)" strokeWidth="1.5">
        <title>{`predicted ${fmtPct(b.predicted)}, observed ${fmtPct(b.observed)}, ${b.runs} runs`}</title>
      </circle>)}
    </svg>
    <div style={{ fontSize: 11, color: 'var(--ink-faint)', lineHeight: '16px' }}>
      Predicted (across) against observed (up) success, depth 3, in tenths. Dots on the diagonal: when the map
      said 10%, about 10% succeeded. Dot area is runs.
    </div>
  </div>;
}

/** A run read against its agent's map: the verdict, the interval it rests
    on, its path, and what earlier runs did next from there. */
function RunForesightBanner({ foresight, run, tenant, go, mismatch, windowId }) {
  if (foresight.error) {
    return <ErrorState what={foresight.error.status === 404 ? `No span carries run ${run}.` : foresight.error.what} next={foresight.error.next} />;
  }
  if (!foresight.data) return <Skeleton height={90} />;
  const f = foresight.data;
  const verdict = FORESIGHT_VERDICT[f.verdict] || { tone: 'neutral', label: f.verdict };
  return <Card data-testid="run-foresight" style={{ borderColor: f.verdict === 'likely_failure' ? 'var(--error)' : 'var(--hairline)' }}>
    <div style={{ display: 'flex', alignItems: 'center', gap: 10, flexWrap: 'wrap', marginBottom: 8 }}>
      <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>run</span>
      <a href={'#/run/' + encodeURIComponent(run)} onClick={(e) => { e.preventDefault(); go(['run', run], tenantParams(tenant)); }}
        style={{ fontFamily: 'var(--font-mono)', fontSize: 13 }}>{run}</a>
      <Tag status={verdict.tone} mono>{verdict.label}</Tag>
      <OutcomeTag outcome={f.outcome} />
      {f.last_open ? <span title="The run may still repeat its last step, so it is compared with every run that repeated it at least as often" style={{ fontSize: 12, color: 'var(--ink-faint)' }}>last step open, pooled</span> : null}
      {f.excluded_self ? <span style={{ fontSize: 12, color: 'var(--ink-faint)' }}>left out of its own statistics</span> : null}
    </div>
    <div style={{ fontSize: 13, lineHeight: '20px', color: 'var(--ink)', marginBottom: 10, textWrap: 'pretty' }}>{f.explanation}</div>
    <div className="tz-reflow-1" style={{ display: 'grid', gridTemplateColumns: 'minmax(0, 1.2fr) minmax(0, 1fr)', gap: 16 }}>
      <div style={{ display: 'grid', gap: 8, minWidth: 0 }}>
        <CiBar rate={f.p_success} ci={f.ci95} base={f.base_rate} width={180} />
        <div style={{ fontSize: 12, color: 'var(--ink-muted)' }}>
          support <Mono>{fmtNum(f.support)}</Mono> of <Mono>{fmtNum(f.cohort_runs)}</Mono> completed runs · needs <Mono>{fmtNum(f.min_support)}</Mono> · matched <Mono>{f.matched_depth}</Mono> of <Mono>{f.path.length}</Mono> tokens
        </div>
        <PathTokens path={f.path} matched={f.matched_depth} max={12} accentIndex={f.divergence ? f.divergence.depth : undefined} />
        {mismatch ? <Caveat>{mismatch}</Caveat> : null}
        {windowId !== '30d' ? <Caveat>Foresight judges a run against the last 30 days; the map shown covers {windowId}.</Caveat> : null}
      </div>
      <NextOptions foresight={f} />
    </div>
  </Card>;
}
