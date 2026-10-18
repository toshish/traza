import React from 'react';
import { api } from '../lib/api.js';
import { navigate, useRead, usePoll } from '../lib/route.js';
import { fmtAgo, fmtTimeNs } from '../lib/format.js';
import { LIMITS, splitFingerprint, tenantParams, usageBars } from '../lib/runs.js';
import { Card, Chip, EmptyState, ErrorState, Eyebrow, LoadingBar, Mono, PanelHead } from '../components/primitives/Chrome.jsx';
import { Tag } from '../components/primitives/Tag.jsx';
import { Modal } from '../components/primitives/Modal.jsx';
import { Button } from '../components/primitives/Button.jsx';
import { Input } from '../components/primitives/Input.jsx';
import { UsageBar } from '../components/runs/UsageBar.jsx';

// Runs held to their limits. The guard computes a verdict for every run from
// the spans it ingests — arithmetic over what was recorded, never a judge —
// and this screen is where a person sees which runs are near a limit, why a
// stopped run stopped, and pulls the brake or releases it.
//
// The listing is the server's memory (GET /v1/fuses/runs never rebuilds), so
// it is cheap to poll; a run the guard is not following does not appear here
// until something asks about it.
//
// The server lists most recently active first, and a stopped run takes no
// more steps, so it sinks: past a page of busy runs it is not in the first
// page at all. Filtering one page in the browser would therefore miss exactly
// the runs someone needs to act on, and count them as zero. So stopped and
// warning runs are each asked for by `state`, and the "all" view puts those
// answers first, then the most recently active page.

const STATES = [
  { id: 'all', label: 'all' },
  { id: 'stop', label: 'stopped' },
  { id: 'warn', label: 'warning' },
  { id: 'continue', label: 'continuing' },
];

const POLL_MS = 5000;

/** Runs each read shows. One more is asked for, so the screen knows when
    there are more than it shows and can say so in the count. */
const LIST_LIMIT = 200;

/** A listing cut to what is shown, and whether the server had more. */
function page(read) {
  const runs = read.data?.runs || [];
  return { runs: runs.slice(0, LIST_LIMIT), more: runs.length > LIST_LIMIT };
}

const runKey = (run) => run.tenant + '\u0000' + run.run;

/** What each chip lists and counts, from the three reads. `stop` and `warn`
    are the server's own filters, complete up to the limit. `continue` has no
    server filter, so it is the most recently active page's continuing runs,
    and its count is marked when that page was full. `all` is stopped, then
    warning, then the active page's other runs, each run once (the three
    reads are separate moments, and a run can change verdict between them). */
export function guardLists(stopped, warned, active) {
  const seen = new Set();
  const all = [];
  for (const run of [...stopped.runs, ...warned.runs, ...active.runs]) {
    const key = runKey(run);
    if (seen.has(key)) continue;
    seen.add(key);
    all.push(run);
  }
  const continuing = active.runs.filter((run) => run.verdict === 'continue');
  return {
    all: { runs: all, more: stopped.more || warned.more || active.more },
    stop: stopped,
    warn: warned,
    continue: { runs: continuing, more: active.more },
  };
}

export const VERDICT_TAG = { stop: 'error', warn: 'warn', continue: 'ok' };

/** Whether this credential may pull the kill switch, reset a run, or edit a
    policy: every POST there needs `rw` (`admin` includes it; an open store
    answers `admin`). Unknown is no — the buttons appear once it is known. */
export function canWrite(identity) {
  return Boolean(identity && (identity.scope === 'rw' || identity.scope === 'admin'));
}

function disabledMessage(error) {
  return error && error.status === 409 && /disabled/.test(error.what || '');
}

export function GuardScreen({ go, params, pushToast, identity }) {
  const operator = Boolean(identity && identity.operator);
  const writer = canWrite(identity);
  const state = STATES.some((s) => s.id === params.get('state')) ? params.get('state') : 'all';
  const tenantFilter = operator ? (params.get('tenant') || '') : '';

  // An empty filter is every tenant (the operator view), so it is left out.
  const list = (filter) => (signal) => api.fuseRuns({
    state: filter, limit: LIST_LIMIT + 1, tenant: tenantFilter || undefined,
  }, signal);
  const stopRead = useRead(list('stop'), [tenantFilter]);
  const warnRead = useRead(list('warn'), [tenantFilter]);
  const activeRead = useRead(list(undefined), [tenantFilter]);
  const reads = [stopRead, warnRead, activeRead];
  const policies = useRead((signal) => api.policies(tenantFilter ? { tenant: tenantFilter } : undefined, signal), [tenantFilter]);
  const reload = () => reads.forEach((read) => read.reload());
  usePoll(reload, POLL_MS);

  const [tripping, setTripping] = React.useState(null);
  const [resetting, setResetting] = React.useState(null);
  const [editing, setEditing] = React.useState(null);
  const [deleting, setDeleting] = React.useState(null);

  const lists = React.useMemo(
    () => guardLists(page(stopRead), page(warnRead), page(activeRead)),
    [stopRead.data, warnRead.data, activeRead.data], // eslint-disable-line react-hooks/exhaustive-deps
  );
  const shown = lists[state].runs;
  const loaded = reads.every((read) => read.data);
  const error = reads.map((read) => read.error).find(Boolean);

  // A filter is a change of focus, not a new place: replace the history
  // entry, so Back leaves the screen instead of walking through keystrokes.
  const setParam = (key, value) => {
    const next = new URLSearchParams(params);
    if (value) next.set(key, value); else next.delete(key);
    navigate(['guard'], Object.fromEntries(next), { replace: true });
  };

  const afterAction = (doc, verb) => {
    reload();
    if (doc && doc.verdict === 'stop' && verb === 'reset') {
      pushToast({
        status: 'warn', title: 'Reset, and stopped again',
        detail: 'Cumulative usage is not refilled by a reset: ' + (doc.reasons || []).join('; ')
          + '. Raise the limit with a grant to let it continue.',
      });
    } else {
      pushToast({ status: 'ok', title: verb === 'trip' ? 'Kill switch on' : 'Run reset', detail: `${doc.run}: ${doc.verdict}` });
    }
  };

  if (disabledMessage(error)) {
    return <EmptyState message={<span>Run fuses are off on this server. It was started with{' '}
      <code>--no-fuses</code>, so ingest does no fuse work and every <code>/v1/fuses</code> route
      answers <code>409</code>. Restart without the flag to watch runs here.</span>} />;
  }

  return <div style={{ display: 'grid', gap: 14, maxWidth: 1560 }}>
    <div style={{ display: 'flex', alignItems: 'center', gap: 6, flexWrap: 'wrap' }}>
      {STATES.map((s) => <Chip key={s.id} active={state === s.id} onClick={() => setParam('state', s.id === 'all' ? '' : s.id)}
        title={lists[s.id].more ? `More than the ${LIST_LIMIT} most recently active ${s.id === 'all' ? 'runs' : s.label + ' runs'} shown` : undefined}>
        {s.label} <span style={{ fontFamily: 'var(--font-mono)', fontVariantNumeric: 'tabular-nums' }}>
          {loaded ? lists[s.id].runs.length + (lists[s.id].more ? '+' : '') : '…'}
        </span>
      </Chip>)}
      {operator ? <span style={{ marginLeft: 8 }}>
        <Input size="sm" mono placeholder="tenant (all)" ariaLabel="Filter by tenant" value={tenantFilter}
          onChange={(value) => setParam('tenant', value.trim())} style={{ width: 150 }} />
      </span> : null}
      <span style={{ marginLeft: 'auto', fontSize: 12, color: 'var(--ink-faint)' }}>
        from the guard's memory, refreshed every {POLL_MS / 1000}s
      </span>
    </div>

    <LoadingBar active={!loaded && !error} />
    {error ? <ErrorState what={error.what} next={error.next} onRetry={reload} /> : null}

    {policies.data?.global ? <GlobalDefault global={policies.data.global} /> : null}

    {loaded && !shown.length ? <EmptyState message={state === 'all'
      ? <span>The guard is following no runs yet. A run is followed from its first step: a model
        call, a tool call or an agent invocation, grouped by session id (or <code>trace:&lt;id&gt;</code>{' '}
        without one). Send one and it appears here.</span>
      : <span>No run is {STATES.find((s) => s.id === state).label} right now.</span>} /> : null}

    <div style={{ display: 'grid', gap: 10 }}>
      {shown.map((run) => <RunCard key={runKey(run)} run={run} operator={operator}
        writer={writer} go={go} onTrip={() => setTripping(run)} onReset={() => setResetting(run)} />)}
    </div>
    {loaded && lists[state].more ? <div style={{ fontSize: 12, color: 'var(--ink-muted)' }}>
      {state === 'continue'
        ? `Continuing runs among the ${LIST_LIMIT} most recently active; more runs are followed than that.`
        : state === 'all'
          ? `Stopped and warning runs first, up to ${LIST_LIMIT} of each, then the ${LIST_LIMIT} most recently active; more runs are followed than shown.`
          : `The ${LIST_LIMIT} most recently active of more ${STATES.find((s) => s.id === state).label} runs.`}
      {operator ? ' Filter by tenant to narrow it.' : null}
    </div> : null}

    <PoliciesPanel policies={policies} operator={operator} writer={writer}
      onEdit={(policy) => setEditing(policy || {})} onDelete={(policy) => setDeleting(policy)} />

    {!writer && identity ? <div style={{ fontSize: 12, color: 'var(--ink-muted)' }}>
      This credential is <code>{identity.scope || 'unknown'}</code>: the kill switch, resets and policy
      edits need <code>rw</code>.
    </div> : null}

    <TripModal run={tripping} onClose={() => setTripping(null)}
      onDone={(doc) => { setTripping(null); afterAction(doc, 'trip'); }} />
    <ResetModal run={resetting} onClose={() => setResetting(null)}
      onDone={(doc) => { setResetting(null); afterAction(doc, 'reset'); }} />
    <PolicyEditor policy={editing} operator={operator} onClose={() => setEditing(null)}
      onSaved={(saved) => {
        setEditing(null);
        policies.reload();
        reload();
        pushToast({ status: 'ok', title: 'Policy saved', detail: `${saved.name} · ${saved.action}` });
      }} />
    <DeletePolicy policy={deleting} onClose={() => setDeleting(null)}
      onDeleted={(policy) => {
        setDeleting(null);
        policies.reload();
        reload();
        pushToast({ status: 'ok', title: 'Policy deleted', detail: policy.name });
      }} />
  </div>;
}

function GlobalDefault({ global }) {
  const limits = Object.entries(global.limits || {});
  return <Card pad="10px 14px">
    <div style={{ display: 'flex', alignItems: 'baseline', gap: 10, flexWrap: 'wrap', fontSize: 13, lineHeight: '20px' }}>
      <Eyebrow>global default</Eyebrow>
      <span style={{ color: 'var(--ink-muted)' }}>
        a run with no matching or <code>default</code> policy gets the server's <code>--fuse-*</code> flags:
      </span>
      <Tag status={global.action === 'stop' ? 'error' : 'warn'} mono>{global.action}</Tag>
      {limits.length
        ? limits.map(([key, value]) => <Mono key={key} color="var(--ink)">{key} {value}</Mono>)
        : <span style={{ color: 'var(--ink-muted)' }}>no limits</span>}
    </div>
  </Card>;
}

function RunCard({ run, operator, writer, go, onTrip, onReset }) {
  const bars = usageBars(run);
  const repeat = run.usage?.top_repeat;
  const repeatParts = repeat ? splitFingerprint(repeat.fingerprint) : null;
  const openRun = () => go(['run', run.run], tenantParams(run.tenant));
  return <Card data-testid="guard-run" data-verdict={run.verdict} pad="12px 14px">
    <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap', marginBottom: 8 }}>
      <button type="button" className="tz-plain-button" onClick={openRun} title="Open the run's tape"
        style={{ fontFamily: 'var(--font-mono)', fontSize: 13, color: 'var(--ink)', cursor: 'pointer', textDecoration: 'underline', textDecorationColor: 'var(--ink-faint)', textUnderlineOffset: 2 }}>
        {run.run}
      </button>
      <Tag status={VERDICT_TAG[run.verdict] || 'neutral'} mono>{run.verdict}</Tag>
      {operator ? <Mono color="var(--ink-muted)">{run.tenant ? 'tenant ' + run.tenant : 'default tenant'}</Mono> : null}
      <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>
        policy <Mono color="var(--ink)">{run.policy}</Mono>
        {run.tripped_at_ns ? <> · tripped {run.tripped_by === 'manual' ? 'by hand' : 'by policy'}{' '}
          <span title={fmtTimeNs(run.tripped_at_ns)}>{fmtAgo(run.tripped_at_ns)} ago</span></> : null}
        {run.grant ? <> · grant <Mono>{Object.entries(run.grant).map(([k, v]) => `${k} ${v}`).join(', ')}</Mono></> : null}
      </span>
      <span style={{ marginLeft: 'auto', display: 'flex', flexWrap: 'wrap', gap: 6 }}>
        {writer && run.verdict === 'stop'
          ? <Chip tone="primary" onClick={onReset}>Approve to continue</Chip> : null}
        {writer && run.verdict !== 'stop'
          ? <Chip onClick={onTrip}>Kill switch</Chip> : null}
        <Chip onClick={openRun}>Tape</Chip>
      </span>
    </div>

    {run.reasons?.length ? <ul style={{ margin: '0 0 10px', paddingLeft: 18, fontSize: 13, lineHeight: '20px' }}>
      {run.reasons.map((reason, i) => <li key={i} style={{
        color: run.verdict === 'stop' ? 'var(--error)' : run.verdict === 'warn' ? 'var(--warn)' : 'var(--ink)',
      }}><span style={{ color: 'var(--ink)' }}>{reason}</span></li>)}
    </ul> : null}

    {bars.length ? <div className="tz-reflow-1" style={{
      display: 'grid', gridTemplateColumns: 'repeat(auto-fill, minmax(300px, 1fr))', gap: '6px 22px',
    }}>
      {bars.map((bar) => <UsageBar key={bar.key} bar={bar} />)}
    </div> : <div style={{ fontSize: 12, color: 'var(--ink-muted)' }}>
      No limits in force: the policy sets none, so the run is unbounded.
    </div>}

    {(repeat || run.evidence?.length) ? <div style={{
      display: 'flex', gap: 14, flexWrap: 'wrap', marginTop: 10, paddingTop: 8,
      borderTop: '1px solid var(--hairline)', fontSize: 12, color: 'var(--ink-muted)',
    }}>
      {repeat ? <span>
        most repeated <Mono color="var(--ink)">{repeatParts.kind}:{repeatParts.name}</Mono>
        {repeatParts.hash ? <Mono color="var(--ink-faint)">#{repeatParts.hash}</Mono> : null}
        {' '}<Mono color="var(--accent)">×{repeat.count}</Mono>
      </span> : null}
      {(run.evidence || []).map((ev, i) => <a key={i} href={'#/trace/' + encodeURIComponent(ev.trace_id) + '?span=' + encodeURIComponent(ev.span_id)}
        onClick={(e) => { e.preventDefault(); go(['trace', ev.trace_id], { span: ev.span_id }); }}>
        evidence <Mono>{ev.trace_id.slice(0, 12)}…/{ev.span_id}</Mono>
      </a>)}
      <span style={{ marginLeft: 'auto', color: 'var(--ink-faint)' }}>source {run.source}</span>
    </div> : null}
  </Card>;
}

function PoliciesPanel({ policies, operator, writer, onEdit, onDelete }) {
  const list = policies.data?.policies || [];
  const columns = operator ? '140px 110px 170px 1fr 70px 132px' : '140px 170px 1fr 70px 132px';
  return <Card pad="0" style={{ overflow: 'hidden' }}>
    <div style={{ padding: '12px 14px 0' }}>
      <PanelHead title="Policies"
        note="exactly one per run: a service match, else the tenant's default, else the global default"
        action={writer ? <Chip tone="primary" onClick={() => onEdit(null)}>New policy</Chip> : null} />
    </div>
    {policies.error ? <div style={{ padding: '0 14px 12px' }}>
      <ErrorState what={policies.error.what} next={policies.error.next} onRetry={policies.reload} />
    </div> : null}
    {policies.data && !list.length ? <div style={{ padding: '0 14px 14px', fontSize: 13, color: 'var(--ink-muted)' }}>
      No policies yet, so every run is held to the global default above.
    </div> : null}
    {list.length ? <div className="tz-scroll-x">
      <div style={{ minWidth: 760 }}>
        <div style={{
          display: 'grid', gridTemplateColumns: columns, background: 'var(--bg-sunken)',
          borderTop: '1px solid var(--hairline)', borderBottom: '1px solid var(--hairline)',
        }}>
          {['name', ...(operator ? ['tenant'] : []), 'matches', 'limits', 'action', ''].map((label, i) => <div key={i} style={{
            padding: '6px 10px', fontSize: 12, fontWeight: 500, color: 'var(--ink-muted)',
          }}>{label}</div>)}
        </div>
        {list.map((policy) => <div key={policy.id} data-testid="policy-row" style={{
          display: 'grid', gridTemplateColumns: columns, alignItems: 'center',
          borderBottom: '1px solid var(--hairline)', minHeight: 'var(--row-h)', fontSize: 'var(--cell-fs)',
        }}>
          <div style={{ padding: 'var(--row-py) 10px' }}><Mono color="var(--ink)">{policy.name}</Mono></div>
          {operator ? <div style={{ padding: 'var(--row-py) 10px' }}><Mono color="var(--ink-muted)">{policy.tenant || '(default)'}</Mono></div> : null}
          <div style={{ padding: 'var(--row-py) 10px', color: 'var(--ink-muted)' }}>
            {policy.match?.service ? <>service <Mono color="var(--ink)">{policy.match.service}</Mono></> : 'every other run'}
          </div>
          <div style={{ padding: 'var(--row-py) 10px', display: 'flex', gap: 10, flexWrap: 'wrap' }}>
            {Object.entries(policy.limits || {}).map(([key, value]) => <Mono key={key} color="var(--ink)">
              <span style={{ color: 'var(--ink-muted)' }}>{key}</span> {value}
            </Mono>)}
            {!Object.keys(policy.limits || {}).length ? <span style={{ color: 'var(--ink-muted)' }}>none</span> : null}
          </div>
          <div style={{ padding: 'var(--row-py) 10px' }}>
            <Tag status={policy.action === 'stop' ? 'error' : 'warn'} mono>{policy.action}</Tag>
          </div>
          <div style={{ padding: 'var(--row-py) 10px', display: 'flex', gap: 6, justifyContent: 'flex-end' }}>
            {writer ? <>
              <Chip onClick={() => onEdit(policy)}>Edit</Chip>
              <Chip onClick={() => onDelete(policy)}>Delete</Chip>
            </> : null}
          </div>
        </div>)}
      </div>
    </div> : null}
  </Card>;
}

function useAction() {
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState(null);
  const run = async (fn) => {
    setBusy(true);
    setError(null);
    try { return await fn(); } catch (e) { setError(e); return undefined; } finally { setBusy(false); }
  };
  return { busy, error, run, clear: () => setError(null) };
}

function ActionError({ error }) {
  if (!error) return null;
  return <ErrorState what={error.what || 'The request failed.'} next={error.next} style={{ marginTop: 10 }} />;
}

export function TripModal({ run, onClose, onDone }) {
  const [reason, setReason] = React.useState('');
  const action = useAction();
  React.useEffect(() => { setReason(''); action.clear(); }, [run]); // eslint-disable-line react-hooks/exhaustive-deps
  if (!run) return null;
  const submit = async () => {
    const doc = await action.run(() => api.fuseTrip(run.run, reason.trim(), tenantParams(run.tenant)));
    if (doc) onDone(doc);
  };
  return <Modal open title="Kill switch" onClose={onClose} footer={<>
    <Button onClick={onClose}>Cancel</Button>
    <Button variant="danger" onClick={submit} disabled={action.busy}>Stop this run</Button>
  </>}>
    <div style={{ display: 'grid', gap: 10 }}>
      <div style={{ color: 'var(--ink-muted)', lineHeight: '19px' }}>
        Stops <Mono color="var(--ink)">{run.run}</Mono> until it is reset, whatever its usage, durably.
        The next ingest acknowledgement and every pre-call check for it answer <code>stop</code>.
        A step already running is not interrupted.
      </div>
      <label style={{ display: 'grid', gap: 4, fontSize: 12, color: 'var(--ink-muted)' }}>reason (optional, kept with the run; leave personal data out)
        <Input value={reason} onChange={setReason} placeholder="customer escalated; taking over by hand"
          onKeyDown={(e) => { if (e.key === 'Enter') submit(); }} />
      </label>
      <ActionError error={action.error} />
    </div>
  </Modal>;
}

/** Turns the editor's text fields into limit values, dropping blanks.
    Returns `{limits, error}`. */
export function parseLimits(fields) {
  const limits = {};
  for (const limit of LIMITS) {
    const text = (fields[limit.key] || '').trim();
    if (!text) continue;
    const value = Number(text);
    if (!Number.isFinite(value) || value <= 0) return { error: `${limit.key} must be a positive number` };
    if (limit.max != null && value > limit.max) return { error: `${limit.key} must be at most ${limit.max}` };
    if (limit.key !== 'max_cost_usd' && !limit.decimal && !Number.isInteger(value)) return { error: `${limit.key} must be a whole number` };
    limits[limit.key] = value;
  }
  return { limits };
}

function LimitFields({ fields, setFields, placeholders }) {
  return <div className="tz-reflow-1" style={{ display: 'grid', gridTemplateColumns: 'repeat(2, minmax(0, 1fr))', gap: 8 }}>
    {LIMITS.map((limit) => <label key={limit.key} style={{ display: 'grid', gap: 3, fontSize: 12, color: 'var(--ink-muted)' }}>
      <Mono color="var(--ink-muted)">{limit.key}</Mono>
      <Input size="sm" mono inputMode="decimal" value={fields[limit.key] || ''} ariaLabel={limit.key}
        placeholder={placeholders?.[limit.key] != null ? String(placeholders[limit.key]) : 'unbounded'}
        onChange={(value) => setFields((f) => ({ ...f, [limit.key]: value }))} />
    </label>)}
  </div>;
}

export function ResetModal({ run, onClose, onDone }) {
  const [fields, setFields] = React.useState({});
  const [invalid, setInvalid] = React.useState('');
  const action = useAction();
  React.useEffect(() => { setFields({}); setInvalid(''); action.clear(); }, [run]); // eslint-disable-line react-hooks/exhaustive-deps
  if (!run) return null;
  const submit = async () => {
    const { limits, error } = parseLimits(fields);
    if (error) { setInvalid(error); return; }
    setInvalid('');
    const doc = await action.run(() => api.fuseReset(run.run, limits, tenantParams(run.tenant)));
    if (doc) onDone(doc);
  };
  return <Modal open title="Approve to continue" width={560} onClose={onClose} footer={<>
    <Button onClick={onClose}>Cancel</Button>
    <Button variant="primary" onClick={submit} disabled={action.busy}>Reset the run</Button>
  </>}>
    <div style={{ display: 'grid', gap: 10 }}>
      <div style={{ color: 'var(--ink-muted)', lineHeight: '19px' }}>
        Clears the stop on <Mono color="var(--ink)">{run.run}</Mono>, with its error streak and repeat
        counts. Cumulative usage is not refilled: a run still past a cost, token, step or duration
        limit stops again at once. To let it go further, grant it higher limits — absolute values,
        for this run only, kept across restarts. Leave a field empty to keep the current limit.
      </div>
      {run.reasons?.length ? <div style={{ fontSize: 12, color: 'var(--error)' }}>{run.reasons.join('; ')}</div> : null}
      {(run.reasons || []).some((reason) => reason.startsWith('foresight:')) ? <div style={{ fontSize: 12, color: 'var(--ink-muted)', lineHeight: '18px' }}>
        A foresight stop is a prediction made again at every check, so a reset alone does not lift it. To let the
        run go on against its history, grant it lower odds: <code>min_success_odds</code> below the upper bound
        in the reason, for example <code>0.05</code>.
      </div> : null}
      <LimitFields fields={fields} setFields={setFields} placeholders={run.limits} />
      {invalid ? <div role="alert" style={{ fontSize: 12, color: 'var(--error)' }}>{invalid}</div> : null}
      <ActionError error={action.error} />
    </div>
  </Modal>;
}

function PolicyEditor({ policy, operator, onClose, onSaved }) {
  const [name, setName] = React.useState('');
  const [tenant, setTenant] = React.useState('');
  const [service, setService] = React.useState('');
  const [actionKind, setActionKind] = React.useState('warn');
  const [fields, setFields] = React.useState({});
  const [invalid, setInvalid] = React.useState('');
  const action = useAction();
  React.useEffect(() => {
    if (!policy) return;
    setName(policy.name || '');
    setTenant(policy.tenant || '');
    setService(policy.match?.service || '');
    setActionKind(policy.action || 'warn');
    setFields(Object.fromEntries(Object.entries(policy.limits || {}).map(([k, v]) => [k, String(v)])));
    setInvalid('');
    action.clear();
  }, [policy]); // eslint-disable-line react-hooks/exhaustive-deps
  if (!policy) return null;
  const isDefault = name.trim() === 'default';
  const submit = async () => {
    const { limits, error } = parseLimits(fields);
    if (error) { setInvalid(error); return; }
    if (!name.trim()) { setInvalid('a policy needs a name'); return; }
    if (!isDefault && !service.trim()) { setInvalid('a policy other than "default" must match a service'); return; }
    setInvalid('');
    const body = { name: name.trim(), limits, action: actionKind };
    if (!isDefault) body.match = { service: service.trim() };
    if (operator && tenant.trim()) body.tenant = tenant.trim();
    const saved = await action.run(() => api.savePolicy(body));
    if (saved) onSaved(saved);
  };
  return <Modal open title={policy.id ? 'Edit policy' : 'New policy'} width={600} onClose={onClose} footer={<>
    <Button onClick={onClose}>Cancel</Button>
    <Button variant="primary" onClick={submit} disabled={action.busy}>Save policy</Button>
  </>}>
    <div style={{ display: 'grid', gap: 10 }}>
      <div style={{ color: 'var(--ink-muted)', lineHeight: '19px' }}>
        Saving replaces the tenant's policy of the same name, and live runs are re-judged at once.
        Limits do not inherit: an absent limit is unbounded under this policy, whatever the default
        or global policy says.
      </div>
      <div className="tz-reflow-1" style={{ display: 'grid', gridTemplateColumns: operator ? '1fr 1fr 1fr' : '1fr 1fr', gap: 8 }}>
        <label style={{ display: 'grid', gap: 3, fontSize: 12, color: 'var(--ink-muted)' }}>name
          <Input size="sm" mono value={name} onChange={setName} placeholder="support or default" />
        </label>
        {operator ? <label style={{ display: 'grid', gap: 3, fontSize: 12, color: 'var(--ink-muted)' }}>tenant
          <Input size="sm" mono value={tenant} onChange={setTenant} placeholder="default tenant" />
        </label> : null}
        <label style={{ display: 'grid', gap: 3, fontSize: 12, color: 'var(--ink-muted)' }}>service its runs start in
          <Input size="sm" mono value={isDefault ? '' : service} onChange={setService} disabled={isDefault}
            placeholder={isDefault ? 'default matches every other run' : 'support-agent'} />
        </label>
      </div>
      <div style={{ display: 'flex', gap: 6, alignItems: 'center' }}>
        <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>at 100% of a limit</span>
        <Chip active={actionKind === 'warn'} onClick={() => setActionKind('warn')}>warn</Chip>
        <Chip active={actionKind === 'stop'} onClick={() => setActionKind('stop')}>stop</Chip>
      </div>
      <LimitFields fields={fields} setFields={setFields} />
      {invalid ? <div role="alert" style={{ fontSize: 12, color: 'var(--error)' }}>{invalid}</div> : null}
      <ActionError error={action.error} />
    </div>
  </Modal>;
}

function DeletePolicy({ policy, onClose, onDeleted }) {
  const action = useAction();
  React.useEffect(() => { action.clear(); }, [policy]); // eslint-disable-line react-hooks/exhaustive-deps
  if (!policy) return null;
  const submit = async () => {
    const result = await action.run(() => api.deletePolicy(policy.id));
    if (result) onDeleted(policy);
  };
  return <Modal open title="Delete policy" onClose={onClose} footer={<>
    <Button onClick={onClose}>Cancel</Button>
    <Button variant="danger" onClick={submit} disabled={action.busy}>Delete {policy.name}</Button>
  </>}>
    <div style={{ color: 'var(--ink-muted)', lineHeight: '19px' }}>
      Its runs fall back to the tenant's <code>default</code> policy, else the global default, and are
      re-judged at once. A sticky stop stays until reset.
    </div>
    <ActionError error={action.error} />
  </Modal>;
}
