import React from 'react';
import { api } from '../../lib/api.js';
import { useRead, usePoll } from '../../lib/route.js';
import { fmtAgo, fmtDurationNs, fmtNum, fmtTimeNs } from '../../lib/format.js';
import { tenantParams } from '../../lib/runs.js';
import {
  NUDGE_MAX_BYTES, NUDGE_TTLS, deliveryWaitMs, fmtInterval, fmtPct, nudgeProblem, utf8Bytes,
} from '../../lib/live.js';
import { Card, Chip, ErrorState, LiveDot, Mono, PanelHead } from '../primitives/Chrome.jsx';
import { Tag } from '../primitives/Tag.jsx';
import { Modal } from '../primitives/Modal.jsx';
import { Button } from '../primitives/Button.jsx';
import { ForesightBar, ForesightChip } from './Foresight.jsx';

// Nudges: words for a running agent, or a veto of its next step, delivered
// once through the pre-call check the agent already makes before each step
// (docs/guide/steer.md#nudges). Queueing one needs `rw`; the message reaches
// the agent as written. Every message shown here is stored text, rendered as
// text — never as markup.

const POLL_PENDING_MS = 1500;

/** A run's nudges, re-read while any is pending (and `poll` is on), so a
    delivery shows up without anyone pressing anything — and, with `follow`
    naming a nudge, until that one is delivered or expired, whether or not
    the last read listed it.

    A read that fails keeps the last list it had (`error` says the refresh
    failed): one dropped connection must neither blank the history nor stop
    the polling that would see the delivery. */
export function useNudges(run, tenant, { enabled = true, poll = true, follow = null } = {}) {
  const read = useRead((signal) => api.nudges(run, tenantParams(tenant), signal), [run, tenant], { skip: !enabled });
  const kept = React.useRef({ scope: null, data: null });
  const scope = enabled ? `${run}\u0000${tenant === undefined ? '\u0001' : tenant}` : null;
  if (kept.current.scope !== scope) kept.current = { scope, data: null };
  if (read.data) kept.current.data = read.data;
  const data = read.data || kept.current.data;
  const nudges = (data && data.nudges) || [];
  const pending = nudges.some((nudge) => nudge.status === 'pending');
  const settled = !follow || nudges.some((nudge) => nudge.id === follow && nudge.status !== 'pending');
  usePoll(read.reload, POLL_PENDING_MS, enabled && poll && (pending || !settled));
  return { ...read, data };
}

/** Whether an error says the store runs with --no-fuses. */
export function fusesOff(error) {
  return Boolean(error && error.status === 409 && /disabled/.test(error.what || ''));
}

const STATUS_TAG = { pending: 'warn', delivered: 'ok', expired: 'neutral' };

/** One nudge's status in words: pending with its expiry, delivered with how
    long it waited for the run's check, or expired. */
export function NudgeStatus({ nudge }) {
  const wait = deliveryWaitMs(nudge);
  return <span style={{ display: 'inline-flex', alignItems: 'center', gap: 6, flexWrap: 'wrap', fontSize: 12, color: 'var(--ink-muted)' }}>
    <Tag status={STATUS_TAG[nudge.status] || 'neutral'} mono>
      {nudge.status === 'pending' ? <LiveDot color="var(--warn)" /> : null}
      {nudge.status}
    </Tag>
    {nudge.status === 'delivered' && wait != null
      ? <span title={fmtTimeNs(nudge.delivered_at_ns)}>after <Mono color="var(--accent)">{fmtDurationNs(wait * 1e6)}</Mono>, at the run's pre-call check</span>
      : nudge.status === 'pending'
        ? <span title={'expires ' + fmtTimeNs(nudge.expires_at_ns)}>waiting for the run's next pre-call check</span>
        : nudge.status === 'expired' ? <span>the run did not check again before it expired</span> : null}
  </span>;
}

/** The run's nudges, newest first. */
export function NudgeHistory({ nudges, highlight }) {
  const list = [...(nudges || [])].reverse();
  if (!list.length) return <div style={{ fontSize: 12, color: 'var(--ink-muted)' }}>No nudges for this run yet.</div>;
  return <ol style={{ listStyle: 'none', margin: 0, padding: 0, display: 'grid', gap: 8 }} aria-label="Nudges">
    {list.map((nudge) => <li key={nudge.id} data-testid="nudge" data-status={nudge.status} style={{
      border: '1px solid var(--hairline)', borderRadius: 'var(--radius-card)', padding: '8px 10px',
      background: nudge.id === highlight ? 'var(--bg-sunken)' : 'var(--bg-raised)', display: 'grid', gap: 5,
    }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
        <Tag status={nudge.kind === 'block' ? 'error' : 'neutral'} mono>{nudge.kind}</Tag>
        <NudgeStatus nudge={nudge} />
        <span title={fmtTimeNs(nudge.created_at_ns)} style={{ marginLeft: 'auto', fontSize: 11, color: 'var(--ink-faint)', fontFamily: 'var(--font-mono)' }}>
          {fmtAgo(nudge.created_at_ns)} ago
        </span>
      </div>
      <div style={{ fontSize: 13, lineHeight: '19px', color: 'var(--ink)', whiteSpace: 'pre-wrap', overflowWrap: 'anywhere' }}>{nudge.message}</div>
    </li>)}
  </ol>;
}

/** The Run screen's card: the run's nudges, and the composer for writers. */
export function NudgesCard({ run, tenant, writer, foresight, onSent }) {
  const [open, setOpen] = React.useState(false);
  const read = useNudges(run, tenant);
  if (fusesOff(read.error)) return null;
  const nudges = read.data?.nudges || [];
  const pending = nudges.filter((n) => n.status === 'pending').length;
  return <Card data-testid="nudges-card">
    <PanelHead title="Nudges" note={pending ? `${pending} waiting for the run's next check` : 'delivered once, through the pre-call check'}
      action={writer ? <Chip tone="primary" onClick={() => setOpen(true)}>Nudge</Chip> : null} />
    {read.error ? <ErrorState what={read.error.what} next={read.error.next} onRetry={read.reload} /> : null}
    {read.data ? <NudgeHistory nudges={nudges} /> : null}
    <NudgeModal target={open ? { run, tenant, foresight } : null} onClose={() => setOpen(false)}
      onSent={(doc) => { read.reload(); onSent?.(doc); }} />
  </Card>;
}

const KINDS = {
  advice: 'Words for the agent, handed to it with its next pre-call check. The guard\'s verdict stands; the agent decides what to do with them.',
  block: 'A veto: the check that delivers it answers stop, with your message as the first reason, and so does every check in the two seconds after, so a batch of calls is refused together. Nothing is tripped. To stop the run until someone resets it, use the kill switch.',
};

/** Compose a nudge, send it, and watch it land.

    `target` is `{run, tenant, agent?, current?, foresight?}`, or null when
    closed. After a send the dialog stays open on the delivery: pending until
    the run's next pre-call check, then delivered (with how long it waited)
    or expired. */
export function NudgeModal({ target, onClose, onSent }) {
  const open = Boolean(target);
  const run = target?.run;
  const tenant = target?.tenant;
  const [kind, setKind] = React.useState('advice');
  const [message, setMessage] = React.useState('');
  const [ttl, setTtl] = React.useState('3600');
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState(null);
  const [sent, setSent] = React.useState(null);
  const [touched, setTouched] = React.useState(false);
  // Held in a ref as well as in state: two presses of Cmd+Enter in one tick
  // both see the state from before the first, and would queue the nudge twice.
  const sending = React.useRef(false);

  React.useEffect(() => {
    setKind('advice'); setMessage(''); setTtl('3600'); setBusy(false); setError(null); setSent(null); setTouched(false);
    sending.current = false;
  }, [run, tenant, open]);

  const history = useNudges(run || '', tenant, { enabled: open, follow: sent ? sent.id : null });
  const fate = useRead((signal) => api.fateRun(run, tenantParams(tenant), signal).catch(() => null),
    [run, tenant], { skip: !open || !target?.foresight || target.foresight.verdict === 'insufficient_history' });

  if (!open) return null;
  const problem = nudgeProblem(message);
  const bytes = utf8Bytes(message);
  const nudges = history.data?.nudges || [];
  const latest = sent ? nudges.find((n) => n.id === sent.id) || sent : null;

  const send = async () => {
    if (sending.current) return;
    setTouched(true);
    if (problem) return;
    sending.current = true;
    setBusy(true);
    setError(null);
    try {
      const body = { message, kind, ttl_seconds: Number(ttl) };
      const doc = await api.nudge(run, body, tenantParams(tenant));
      setSent(doc);
      setMessage('');
      setTouched(false);
      history.reload();
      onSent?.(doc);
    } catch (e) {
      setError(e);
    } finally { sending.current = false; setBusy(false); }
  };

  return <Modal open title={`Nudge ${run}`} width={640} onClose={onClose} footer={<>
    <Button onClick={onClose}>{sent ? 'Done' : 'Cancel'}</Button>
    <Button variant={kind === 'block' ? 'danger' : 'primary'} onClick={send} disabled={busy}>
      {busy ? 'Sending…' : kind === 'block' ? 'Send veto' : 'Send nudge'}
    </Button>
  </>}>
    <div style={{ display: 'grid', gap: 12 }}>
      {target.foresight ? <ForesightContext foresight={target.foresight} fate={fate.data} current={target.current} /> : null}

      {latest ? <div data-testid="nudge-delivery" role="status" style={{
        display: 'grid', gap: 4, padding: '8px 10px', borderRadius: 'var(--radius-card)',
        border: `1px solid ${latest.status === 'delivered' ? 'var(--ok)' : latest.status === 'expired' ? 'var(--hairline)' : 'var(--warn)'}`,
        background: latest.status === 'delivered' ? 'var(--ok-tint)' : latest.status === 'expired' ? 'var(--bg-sunken)' : 'var(--warn-tint)',
      }}>
        <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap', fontSize: 12 }}>
          <span style={{ color: 'var(--ink)', fontWeight: 500 }}>Queued <Mono>{latest.id}</Mono></span>
          <NudgeStatus nudge={latest} />
        </div>
        <div style={{ fontSize: 12, color: 'var(--ink-muted)', lineHeight: '18px' }}>
          Delivered at most once. A run that is not taking steps hears it at its next one; an answer lost
          on the way (a timeout, a crashed client) loses it, though it still lists as delivered.
        </div>
      </div> : null}

      <div role="group" aria-label="Kind of nudge" style={{ display: 'flex', gap: 6, alignItems: 'center' }}>
        {['advice', 'block'].map((k) => <Chip key={k} mono active={kind === k} onClick={() => setKind(k)}
          title={kind === k ? k + ' (selected)' : 'Make this ' + (k === 'block' ? 'a veto' : 'advice')}>{k}</Chip>)}
        <span style={{ fontSize: 12, color: 'var(--ink-faint)', marginLeft: 4 }}>{kind === 'block' ? 'a veto of the next step' : 'words for the agent'}</span>
      </div>
      <div style={{ fontSize: 12, color: 'var(--ink-muted)', lineHeight: '18px' }}>{KINDS[kind]}</div>

      <label style={{ display: 'grid', gap: 4 }}>
        <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>message</span>
        <span className="tz-field" style={{ display: 'block', border: `1px solid ${touched && problem ? 'var(--error)' : 'var(--hairline)'}`, borderRadius: 'var(--radius-control)', background: 'var(--bg-raised)' }}>
          <textarea value={message} onChange={(e) => setMessage(e.target.value)} rows={4}
            aria-label="Nudge message" aria-invalid={touched && Boolean(problem)}
            placeholder={kind === 'block' ? 'Hold refunds over $500 for a human.' : 'The order search keeps coming back empty; stop searching and issue the refund.'}
            onKeyDown={(e) => { if ((e.metaKey || e.ctrlKey) && e.key === 'Enter') send(); }}
            style={{
              display: 'block', width: '100%', resize: 'vertical', border: 'none', outline: 'none', background: 'transparent',
              color: 'var(--ink)', font: 'inherit', fontSize: 13, lineHeight: '19px', padding: '7px 10px', minHeight: 84,
            }} />
        </span>
      </label>
      <div style={{ display: 'flex', alignItems: 'center', gap: 6, flexWrap: 'wrap' }}>
        <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>expires after</span>
        {NUDGE_TTLS.map((option) => <Chip key={option.id} active={ttl === option.id} onClick={() => setTtl(option.id)}>{option.label}</Chip>)}
        <span style={{ marginLeft: 'auto', fontFamily: 'var(--font-mono)', fontSize: 11, fontVariantNumeric: 'tabular-nums', color: bytes > NUDGE_MAX_BYTES ? 'var(--error)' : 'var(--ink-faint)' }}>
          {fmtNum(bytes)} / {fmtNum(NUDGE_MAX_BYTES)} bytes
        </span>
      </div>
      {touched && problem ? <div role="alert" style={{ fontSize: 12, color: 'var(--error)' }}>{problem}</div> : null}
      {error ? <ErrorState what={error.what || 'The nudge was not queued.'} next={error.status === 403
        ? 'Queueing a nudge needs an rw credential: a nudge is operator text, delivered to the agent as written.'
        : error.next} /> : null}

      <div style={{ display: 'grid', gap: 6 }}>
        <div style={{ fontSize: 11, textTransform: 'uppercase', letterSpacing: 'var(--tracking-caps)', color: 'var(--ink-muted)', fontWeight: 500 }}>this run's nudges</div>
        {history.error && !fusesOff(history.error) ? <ErrorState what={history.error.what} next={history.error.next} /> : null}
        {history.data ? <NudgeHistory nudges={nudges} highlight={sent?.id} /> : null}
      </div>
      <div style={{ fontSize: 11, color: 'var(--ink-faint)', lineHeight: '16px' }}>
        Any <code>rw</code> credential can queue a nudge, and its text reaches the agent as written. Keep
        personal data out of it, as you would out of a kill-switch reason.
      </div>
    </div>
  </Modal>;
}

/** What earlier runs like this one did: the foresight, and — when the run
    route answers — what they did next from here, the best first. Context
    for writing the nudge, not a recommendation of Traza's own: every figure
    is a count over stored runs. */
function ForesightContext({ foresight, fate, current }) {
  const next = (fate && fate.next) || [];
  return <div style={{ display: 'grid', gap: 6, padding: '8px 10px', border: '1px solid var(--hairline)', borderRadius: 'var(--radius-card)', background: 'var(--bg)' }}>
    <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
      <span style={{ fontSize: 11, textTransform: 'uppercase', letterSpacing: 'var(--tracking-caps)', color: 'var(--ink-muted)', fontWeight: 500 }}>foresight</span>
      <ForesightChip foresight={foresight} />
      {current ? <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>
        {current.pending ? 'about to run' : 'on'} <Mono>{current.kind}:{current.name || 'next call'}</Mono>
      </span> : null}
    </div>
    <ForesightBar foresight={foresight} />
    {fate && fate.explanation ? <div style={{ fontSize: 12, color: 'var(--ink)', lineHeight: '18px' }}>{fate.explanation}.</div> : null}
    {next.length ? <div style={{ display: 'grid', gap: 3 }}>
      <div style={{ fontSize: 12, color: 'var(--ink-muted)' }}>what those runs did next</div>
      {next.slice(0, 4).map((option) => <div key={option.token + (option.repeat ? ':r' : '')} style={{ display: 'flex', gap: 8, alignItems: 'baseline', fontSize: 12 }}>
        <Mono color={option.token === fate.recommended_next ? 'var(--ink)' : 'var(--ink-muted)'}>{option.token}</Mono>
        {option.repeat ? <span style={{ fontSize: 11, color: 'var(--ink-faint)' }}>again</span> : null}
        <Mono color="var(--accent)">{fmtPct(option.success_rate)}</Mono>
        <Mono color="var(--ink-faint)">{fmtInterval(option.ci95)} · {fmtNum(option.visits)} runs</Mono>
        {option.token === fate.recommended_next ? <Tag status="ok" mono>did best</Tag> : null}
      </div>)}
    </div> : null}
  </div>;
}
