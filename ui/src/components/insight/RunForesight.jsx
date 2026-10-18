import React from 'react';
import { api } from '../../lib/api.js';
import { useRead } from '../../lib/route.js';
import { fmtNum } from '../../lib/format.js';
import { tenantParams } from '../../lib/runs.js';
import { FORESIGHT_VERDICT } from '../../lib/insight.js';
import { Card, Chip, LoadingBar, Mono, PanelHead } from '../primitives/Chrome.jsx';
import { Tag } from '../primitives/Tag.jsx';
import { CiBar, PathTokens, RunSkill } from './Bits.jsx';
import { NextOptions } from './NextOptions.jsx';

// The Run screen's foresight panel: how earlier runs of this agent that took
// the same path ended, what they did next from here, how far the map's
// verdicts at that depth have been worth trusting, and a way onto the Fate
// Map with this run lit up. It is context, like the guard verdict: a run
// with no history to compare against still has its tape.
//
// Foresight is one tenant's, and its route reads the default tenant when
// none is named. A tape an operator reads with no tenant named is every
// tenant's, so — as with the guard verdict — nothing is read until the
// tenant is named (`oneTenant`), rather than another tenant's run of the
// same id being judged beside this tape.

export function RunForesight({ runId, tenant, oneTenant = true, identityKnown = true, go }) {
  const answer = useRead((signal) => api.fateRun(runId, tenantParams(tenant), signal), [runId, tenant, oneTenant],
    { skip: !oneTenant });
  const f = answer.data;
  // The calibration is the build's, whatever is drawn: the default-window
  // map (the build foresight judged against, cached per tenant), drawn as
  // small as the route allows.
  const scored = useRead((signal) => api.fateMap({ agent: f.agent, depth: 1, max_nodes: 1, ...tenantParams(f.tenant) }, signal),
    [f && f.agent, f && f.tenant], { skip: !f || !f.cohort_runs || !(f.matched_depth > 0) });
  const open = () => {
    const params = { run: runId };
    if (f) params.agent = f.agent;
    if (typeof tenant === 'string') params.tenant = tenant;
    go(['foresight'], params);
  };
  return <Card data-testid="run-foresight-panel">
    <PanelHead title="Foresight" note="this path, against the agent's last 30 days"
      action={f ? <Chip onClick={open}>On the Fate Map</Chip> : null} />
    <LoadingBar active={answer.loading || (!oneTenant && !identityKnown)} />
    {!oneTenant && identityKnown ? <div style={{ fontSize: 12, color: 'var(--ink-muted)', lineHeight: '18px' }}>
      Foresight reads one tenant, and no tenant is named: name the run's tenant above to judge it against its
      agent's map.
    </div> : null}
    {answer.error ? <div style={{ fontSize: 12, color: 'var(--ink-muted)', lineHeight: '18px' }}>
      {answer.error.status === 404 ? 'No foresight: the Fate Map holds no span of this run.'
        : answer.error.status === 409 || answer.error.status === 400 ? answer.error.what
          : `Foresight could not be read: ${answer.error.what}`}
    </div> : null}
    {f ? <Body f={f} calibration={scored.data ? scored.data.calibration : null} /> : null}
  </Card>;
}

function Body({ f, calibration }) {
  const verdict = FORESIGHT_VERDICT[f.verdict] || { tone: 'neutral', label: f.verdict };
  return <div style={{ display: 'grid', gap: 10 }}>
    <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap' }}>
      <Tag status={verdict.tone} mono>{verdict.label}</Tag>
      <Mono size={12} color="var(--ink-muted)">{f.agent}</Mono>
      {f.last_open ? <span style={{ fontSize: 11, color: 'var(--ink-faint)' }} title="The run may still repeat its last step, so it is compared with every run that repeated it at least as often">last step open</span> : null}
    </div>
    <div style={{ fontSize: 12, lineHeight: '18px', color: 'var(--ink)', textWrap: 'pretty' }}>{f.explanation}</div>
    {f.ci95 ? <CiBar rate={f.p_success} ci={f.ci95} base={f.base_rate} width={150} /> : null}
    {f.cohort_runs ? <div style={{ fontSize: 12, color: 'var(--ink-muted)' }}>
      support <Mono>{fmtNum(f.support)}</Mono> of <Mono>{fmtNum(f.cohort_runs)}</Mono> runs · matched <Mono>{f.matched_depth}</Mono> of <Mono>{f.path.length}</Mono> tokens
    </div> : null}
    {calibration ? <RunSkill calibration={calibration} depth={f.matched_depth} /> : null}
    {f.path.length ? <PathTokens path={f.path} matched={f.matched_depth} max={8} accentIndex={f.divergence ? f.divergence.depth : undefined} /> : null}
    {f.cohort_runs ? <NextOptions foresight={f} /> : null}
  </div>;
}
