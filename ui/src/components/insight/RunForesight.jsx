import React from 'react';
import { api } from '../../lib/api.js';
import { useRead } from '../../lib/route.js';
import { fmtNum } from '../../lib/format.js';
import { tenantParams } from '../../lib/runs.js';
import { FORESIGHT_VERDICT } from '../../lib/insight.js';
import { Card, Chip, LoadingBar, Mono, PanelHead } from '../primitives/Chrome.jsx';
import { Tag } from '../primitives/Tag.jsx';
import { CiBar, PathTokens } from './Bits.jsx';
import { NextOptions } from './NextOptions.jsx';

// The Run screen's foresight panel: how earlier runs of this agent that took
// the same path ended, what they did next from here, and a way onto the
// Fate Map with this run lit up. It is context, like the guard verdict: a
// run with no history to compare against still has its tape.

export function RunForesight({ runId, tenant, go }) {
  const answer = useRead((signal) => api.fateRun(runId, tenantParams(tenant), signal), [runId, tenant]);
  const open = () => {
    const params = { run: runId };
    if (answer.data) params.agent = answer.data.agent;
    if (typeof tenant === 'string') params.tenant = tenant;
    go(['foresight'], params);
  };
  return <Card data-testid="run-foresight-panel">
    <PanelHead title="Foresight" note="this path, against the agent's last 30 days"
      action={answer.data ? <Chip onClick={open}>On the Fate Map</Chip> : null} />
    <LoadingBar active={answer.loading} />
    {answer.error ? <div style={{ fontSize: 12, color: 'var(--ink-muted)', lineHeight: '18px' }}>
      {answer.error.status === 404 ? 'No foresight: the Fate Map holds no span of this run.'
        : answer.error.status === 409 || answer.error.status === 400 ? answer.error.what
          : `Foresight could not be read: ${answer.error.what}`}
    </div> : null}
    {answer.data ? <Body f={answer.data} /> : null}
  </Card>;
}

function Body({ f }) {
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
    {f.path.length ? <PathTokens path={f.path} matched={f.matched_depth} max={8} accentIndex={f.divergence ? f.divergence.depth : undefined} /> : null}
    {f.cohort_runs ? <NextOptions foresight={f} /> : null}
  </div>;
}

