import React from 'react';
import { api } from '../lib/api.js';
import { hashFor, navigate, useMedia } from '../lib/route.js';
import { fmtCompact, fmtNum, fmtTimeNs } from '../lib/format.js';
import { tenantParams } from '../lib/runs.js';
import {
  FILTERS, ROW_CAP, SORTS, ageOut, agentsOf, applyRemoved, applyRun, applySnapshot, foresightSentence, guardState, idleMs,
  isPartial, isQuiet, keepListed, liveTotals, matches, newLiveState, probeCandidates, rowKey, runLiveStream, serverNowMs,
  sortEntries, stepElapsedMs, waitingKeys,
} from '../lib/live.js';
import { Card, Chip, EmptyState, ErrorState, LiveDot, Mono, Skeleton } from '../components/primitives/Chrome.jsx';
import { Tag } from '../components/primitives/Tag.jsx';
import { Input } from '../components/primitives/Input.jsx';
import { ForesightBar, ForesightCell, ForesightChip } from '../components/runs/Foresight.jsx';
import { NudgeHistory, NudgeModal, useNudges } from '../components/runs/Nudges.jsx';
import { ResetModal, TripModal, canWrite } from './GuardScreen.jsx';

// Mission Control: the runs working right now, what each is doing, what it
// has spent, what the guard says, how runs like it have ended — and the
// wheel: a nudge, the kill switch, an approval to go on.
//
// One held connection carries it (GET /v1/runs/live/stream): a snapshot,
// then a row each time a run changes, and a removal when one leaves the view
// (the guard let go of it, or it went quiet past the live window). When the stream is refused — a scope
// holds at most eight — the screen reads the listing every five seconds
// instead, and says so. Everything shown is the guard's memory, the same
// state the pre-call check answers from: nothing here is estimated, and
// foresight is counting over stored runs, not a model's guess.

/** How often the held-run probe looks for runs that asked to take a step
    and have not taken it. */
const PROBE_MS = 3000;
/** How long a named step must stay pending before the probe asks why. */
const PROBE_AFTER_MS = 4000;
/** Verdict reads in flight at once, at most: each is one read from memory.
    A round asks only as many as have finished. */
const PROBE_BATCH = 4;
/** How long a probe's answer stands before the run is asked about again. */
const PROBE_TTL_MS = 10_000;

const ClockContext = React.createContext(0);

/** The client clock, ticking once a second while the tab is visible. */
function useNow(intervalMs = 1000) {
  const [now, setNow] = React.useState(() => Date.now());
  React.useEffect(() => {
    const timer = setInterval(() => { if (!document.hidden) setNow(Date.now()); }, intervalMs);
    return () => clearInterval(timer);
  }, [intervalMs]);
  return now;
}

/** Follows the live view for one scope: rows, connection state, and a note
    when the view was rebuilt from a fresh snapshot. `updatedAt` is when a
    snapshot or poll last landed in this scope — null until the first does,
    so the screen never states anything about the runs before it has read
    them. `retry()` starts over after a refusal. */
function useLiveRuns(tenant) {
  const [state, setState] = React.useState(newLiveState);
  const [status, setStatus] = React.useState({ state: 'connecting' });
  const [updatedAt, setUpdatedAt] = React.useState(null);
  const [note, setNote] = React.useState(null);
  const [breaks, setBreaks] = React.useState(0);
  const [attempt, setAttempt] = React.useState(0);

  React.useEffect(() => {
    const controller = new AbortController();
    setState(newLiveState());
    setStatus({ state: 'connecting' });
    setUpdatedAt(null);
    setNote(null);
    setBreaks(0);
    let broken = false;
    let seen = false;
    runLiveStream({
      // The runner's own signal per connection, so a connection it gives up
      // on (silent past the heartbeat) is closed, not left holding a socket.
      open: (params, signal) => api.liveChunks(params, signal),
      poll: (params) => api.liveRuns(params, controller.signal),
      params: tenant ? { tenant } : {},
      signal: controller.signal,
      onSnapshot: (snapshot, { source }) => {
        const now = Date.now();
        setState((current) => applySnapshot(current, snapshot, now));
        setUpdatedAt(now);
        if (source !== 'stream') return;
        // There is no resume cursor: a reconnect starts from a snapshot that
        // replaces everything, which heals the break. Said once, briefly.
        if (broken) {
          setBreaks((n) => n + 1);
          setNote({ kind: 'rebuilt', at: now });
        } else if (seen) {
          setNote({ kind: 'resent', at: now });
        }
        broken = false;
        seen = true;
      },
      onRun: (row) => {
        const now = Date.now();
        setState((current) => applyRun(current, row, now));
        setUpdatedAt(now);
      },
      onRemoved: (removal) => {
        setState((current) => applyRemoved(current, removal));
        setUpdatedAt(Date.now());
      },
      onStatus: (next) => {
        if (next.state === 'reconnecting') broken = true;
        setStatus(next);
      },
    });
    const ager = setInterval(() => setState((current) => ageOut(current, Date.now())), 5000);
    return () => { controller.abort(); clearInterval(ager); };
  }, [tenant, attempt]);

  React.useEffect(() => {
    if (!note) return undefined;
    const timer = setTimeout(() => setNote(null), 8000);
    return () => clearTimeout(timer);
  }, [note]);

  const retry = React.useCallback(() => setAttempt((n) => n + 1), []);
  return { state, status, updatedAt, note, breaks, retry };
}

/** Finds the runs the live listing cannot say are held — as far as the
    verdict route can say it.

    A predictive stop — `min_success_odds` — is evaluated on the pre-call
    check and the verdict route only, never into the guard's listing. A run
    it holds shows its next step pending, and nothing else. So a run whose
    named step has stayed pending a few seconds is asked about, with the
    verdict route: a read from memory that is neither activity nor a change,
    and never takes the run's nudges. That route judges the path so far,
    without the step the check was asked about, so a hold that exists only
    for that step — a predictive limit judging the named tool, a budget its
    estimate would cross, a block nudge — is not found here; such a run is
    shown as waiting instead. At most four reads in flight at once, and a
    run never in two: a round asks only as many runs as there is room for,
    so slow answers hold the probe back rather than piling up behind it.
    Each run at most once per ten seconds or per move it makes, the runs
    never asked first and then the longest waiting. `forget(key)` drops what
    is known about a run, so it is asked again at the next round (after an
    approval, say).

    What it knows follows the rows: a run that leaves the view takes with it
    when it was asked, the stop found for it, and a read still in flight
    about it (cancelled). */
function useHeldProbe(state, serverNow) {
  const [held, setHeld] = React.useState(() => new Map());
  const latest = React.useRef({ state, serverNow });
  latest.current = { state, serverNow };
  const asked = React.useRef(new Map());
  // Reads in flight, by run, each with its own controller.
  const flying = React.useRef(new Map());
  const cancel = React.useCallback((key) => {
    const read = flying.current.get(key);
    if (!read) return;
    read.abort();
    flying.current.delete(key);
  }, []);
  const forget = React.useCallback((key) => {
    asked.current.delete(key);
    cancel(key);
    setHeld((map) => {
      if (!map.has(key)) return map;
      const next = new Map(map);
      next.delete(key);
      return next;
    });
  }, [cancel]);

  React.useEffect(() => {
    let live = true;
    const round = () => {
      if (document.hidden) return;
      const { state: current, serverNow: now } = latest.current;
      asked.current = keepListed(asked.current, current.rows);
      for (const key of [...flying.current.keys()]) if (!current.rows.has(key)) cancel(key);
      setHeld((map) => keepListed(map, current.rows));
      const room = PROBE_BATCH - flying.current.size;
      if (room <= 0) return;
      const candidates = probeCandidates(current.rows, asked.current, now, Date.now(),
        { afterMs: PROBE_AFTER_MS, ttlMs: PROBE_TTL_MS, batch: current.rows.size })
        .filter(({ key }) => !flying.current.has(key)).slice(0, room);
      for (const { key, row } of candidates) {
        const ask = { activity: row.last_activity_ns, at: Date.now() };
        asked.current.set(key, ask);
        const read = new AbortController();
        flying.current.set(key, read);
        api.fuseRun(row.run, tenantParams(row.tenant || ''), read.signal).then((doc) => {
          // An answer to a question since forgotten (an approval in between,
          // or the run left the view) or asked again is out of date.
          if (!live || asked.current.get(key) !== ask) return;
          setHeld((map) => {
            const next = new Map(map);
            if (doc && doc.verdict === 'stop') next.set(key, { doc, activity: row.last_activity_ns });
            else next.delete(key);
            return next;
          });
        }, () => { /* no answer is no evidence of a hold */ }).finally(() => {
          if (flying.current.get(key) === read) flying.current.delete(key);
        });
      }
    };
    const timer = setInterval(round, PROBE_MS);
    const reads = flying.current;
    return () => {
      live = false;
      clearInterval(timer);
      for (const read of reads.values()) read.abort();
      reads.clear();
    };
  }, [cancel]);
  return { held, forget };
}

/** "4 s", "1m 04s", "2h 05m": a running step's age at a glance. Whole
    seconds, because it is redrawn once a second. */
export function fmtElapsed(ms) {
  if (ms == null || !Number.isFinite(ms)) return '—';
  const s = Math.floor(ms / 1000);
  if (s < 60) return `${s} s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${String(s % 60).padStart(2, '0')}s`;
  return `${Math.floor(m / 60)}h ${String(m % 60).padStart(2, '0')}m`;
}

/** Dollars at a fixed four places, so a ticking figure does not change width
    as it grows. */
export function fmtUsd(value) {
  if (typeof value !== 'number' || !Number.isFinite(value)) return '—';
  return Math.abs(value) >= 100 ? value.toFixed(2) : value.toFixed(4);
}

/** A measured figure that counts up to its new value instead of jumping,
    unless the reader asked for reduced motion (or there is no screen to
    animate on). The exact value is always in the title. */
function Ticking({ value, format, style }) {
  const [shown, setShown] = React.useState(value);
  const shownRef = React.useRef(value);
  shownRef.current = shown;
  React.useEffect(() => {
    const from = shownRef.current;
    if (from === value) return undefined;
    const motion = typeof window !== 'undefined' && typeof window.matchMedia === 'function'
      && typeof window.requestAnimationFrame === 'function'
      && !window.matchMedia('(prefers-reduced-motion: reduce)').matches;
    if (!motion || typeof from !== 'number' || typeof value !== 'number') { setShown(value); return undefined; }
    const start = performance.now();
    let frame = 0;
    const step = (t) => {
      const k = Math.min(1, (t - start) / 650);
      setShown(from + (value - from) * (1 - (1 - k) ** 3));
      if (k < 1) frame = window.requestAnimationFrame(step);
    };
    frame = window.requestAnimationFrame(step);
    return () => window.cancelAnimationFrame(frame);
  }, [value]);
  return <span title={typeof value === 'number' ? String(value) : undefined} style={style}>{format(shown)}</span>;
}

/** How long the current step has been pending, from the shared clock — or,
    for a recorded step, which has already ended (spans arrive when they
    end), how long ago the run's latest span arrived. */
function Elapsed({ row }) {
  const serverNow = React.useContext(ClockContext);
  const idle = idleMs(row, serverNow);
  if (row.current && row.current.pending) {
    return <span title={'named by a pre-call check at ' + fmtTimeNs(row.current.started_ns) + '; no span since'}>
      {fmtElapsed(stepElapsedMs(row, serverNow))}
    </span>;
  }
  if (isQuiet(row, serverNow)) {
    return <span title={'last activity ' + fmtTimeNs(row.last_activity_ns)} style={{ color: 'var(--ink-faint)' }}>idle {fmtElapsed(idle)}</span>;
  }
  return <span title={'no step pending; this step has ended, and the run\'s latest span arrived at ' + fmtTimeNs(row.last_activity_ns)}
    style={{ color: 'var(--ink-faint)' }}>{fmtElapsed(idle)} ago</span>;
}

/** A row that just changed is marked for a moment, so the eye finds what
    moved without reading every figure. Keyed by the change, so each change
    plays once; a row remounted later does not replay an old one. */
function Flash({ at }) {
  if (!at || Date.now() - at > 1500) return null;
  return <span key={at} aria-hidden="true" className="tz-flash" />;
}

const GUARD = {
  continue: { tag: 'ok', label: 'continue' },
  warn: { tag: 'warn', label: 'warn' },
  stop: { tag: 'error', label: 'stop' },
  held: { tag: 'error', label: 'held' },
  waiting: { tag: 'warn', label: 'waiting' },
};

/** What `waiting` means, in a sentence: the page knows the run asked, and
    not what it was told. */
function waitingSentence(row) {
  const step = row.current ? `${row.current.kind}${row.current.name ? ':' + row.current.name : ''}` : 'its next step';
  return `It asked the pre-call check about ${step} and has recorded nothing since. The listing's verdict, `
    + `${row.verdict || 'continue'}, judges the path so far; the check judged the path with that step added, and its `
    + 'answer is not in the listing. The step may still be running, or the check refused it — a predictive limit '
    + 'judging that tool, a budget the step\'s estimate would cross, a block nudge — and which of the two cannot be '
    + 'read from here.';
}

function GuardChip({ entry }) {
  const state = guardState(entry);
  const known = GUARD[state] || { tag: 'neutral', label: state };
  const reasons = state === 'held' ? (entry.held.doc.reasons || []) : (entry.row.reasons || []);
  const title = state === 'waiting' ? waitingSentence(entry.row)
    : reasons.length ? reasons.join('\n') : `the guard says ${state}`;
  // `continue` is the ordinary state: in ink, so the exceptions are what the
  // eye finds in a column of them.
  return <span title={title}>
    {state === 'continue'
      ? <span style={{ fontFamily: 'var(--font-mono)', fontSize: 12, color: 'var(--ink-muted)' }}>continue</span>
      : <Tag status={known.tag} mono>{known.label}</Tag>}
  </span>;
}

function StepCell({ row }) {
  const current = row.current;
  if (!current) return <span style={{ color: 'var(--ink-faint)' }}>—</span>;
  const name = current.name || (current.kind === 'llm' ? 'next model call' : current.kind);
  return <div style={{ display: 'flex', alignItems: 'center', gap: 6, minWidth: 0 }}>
    <Tag mono style={{ flex: 'none' }}>{current.kind}</Tag>
    <span title={current.pending ? 'Named by a pre-call check; no span has arrived since' : 'The latest-starting recorded step'}
      style={{ fontFamily: 'var(--font-mono)', fontSize: 12, color: 'var(--ink)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', minWidth: 0 }}>
      {name}
    </span>
    {current.pending ? <LiveDot color="var(--accent)" style={{ flex: 'none' }} /> : null}
    <span style={{ marginLeft: 'auto', flex: 'none', fontFamily: 'var(--font-mono)', fontSize: 11, fontVariantNumeric: 'tabular-nums', color: 'var(--ink-muted)' }}>
      <Elapsed row={row} />
    </span>
  </div>;
}

/** A run's tape, in its own tenant — the address `go` opens, so a link
    opened in a new tab lands in the same place. */
function runHref(row) {
  return hashFor(['run', row.run], tenantParams(row.tenant || ''));
}

const COLUMNS = 'minmax(140px, 1.1fr) minmax(190px, 1.35fr) 52px 66px 80px 84px minmax(180px, 1fr) 82px 222px';
const HEADERS = ['run', 'now', 'steps', 'tokens', 'cost USD', 'guard', 'foresight', 'nudges', ''];

/** One run as a table row, with its detail beneath when open. Memoised on
    its entry: the shared clock redraws only the elapsed cell each second. */
const LiveRow = React.memo(function LiveRow({ entry, idle, open, operator, writer, onToggle, onAction }) {
  const row = entry.row;
  const key = rowKey(row);
  const guard = guardState(entry);
  return <div data-testid="live-row" data-run={row.run} data-guard={guard}>
    <div role="row" tabIndex={0} aria-expanded={open} onClick={() => onToggle(key)}
      onKeyDown={(e) => {
        // Keys pressed on the row's own controls — the run link, a button —
        // are theirs: Enter on the link follows it.
        if (e.target !== e.currentTarget) return;
        if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); onToggle(key); }
      }}
      style={{
        display: 'grid', gridTemplateColumns: COLUMNS, alignItems: 'center', cursor: 'pointer', position: 'relative', isolation: 'isolate',
        borderBottom: '1px solid var(--hairline)', minHeight: 'var(--row-h)', fontSize: 'var(--cell-fs)',
        opacity: idle ? 0.62 : 1, background: open ? 'var(--bg-sunken)' : 'transparent',
        borderLeft: `2px solid ${guard === 'stop' || guard === 'held' ? 'var(--error)' : row.foresight?.verdict === 'likely_failure' ? 'var(--error-tint)' : 'transparent'}`,
      }}>
      <Flash at={entry.flashAt} />
      <Cell>
        <div style={{ minWidth: 0 }}>
          <a href={runHref(row)} onClick={(e) => { e.preventDefault(); e.stopPropagation(); onAction('open', entry); }}
            style={{ fontFamily: 'var(--font-mono)', fontSize: 12, display: 'block', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{row.run}</a>
          <div style={{ fontSize: 11, color: 'var(--ink-muted)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
            {row.agent}{operator ? <span style={{ color: 'var(--ink-faint)' }}> · {row.tenant ? row.tenant : 'default'}</span> : null}
          </div>
        </div>
      </Cell>
      <Cell><StepCell row={row} /></Cell>
      <Num>{fmtNum(row.steps)}</Num>
      <Num muted>{fmtCompact(row.tokens)}</Num>
      <Num accent><Ticking value={row.cost_usd} format={fmtUsd} /></Num>
      <Cell><GuardChip entry={entry} /></Cell>
      <Cell><ForesightCell foresight={row.foresight} deferred={Boolean(row.foresight_deferred)} /></Cell>
      <Cell align="right">
        {row.pending_nudges ? <Tag status="warn" mono>{row.pending_nudges} waiting</Tag> : <span style={{ color: 'var(--ink-faint)' }}>—</span>}
      </Cell>
      <Cell align="right">
        <Actions entry={entry} writer={writer} onAction={onAction} />
      </Cell>
    </div>
    {open ? <RowDetail entry={entry} writer={writer} onAction={onAction} /> : null}
  </div>;
});

/** The row's buttons. A kill switch for a running run, an approval for a
    stopped or held one, or one waiting on a step it asked about (whose check
    may have refused it) — both need `rw` — and the nudge. */
function Actions({ entry, writer, onAction, wrap = false }) {
  const guard = guardState(entry);
  const act = (name) => (e) => { if (e && e.stopPropagation) e.stopPropagation(); onAction(name, entry); };
  return <span style={{ display: 'inline-flex', gap: 6, flexWrap: wrap ? 'wrap' : 'nowrap', justifyContent: 'flex-end' }}>
    {writer ? <Chip onClick={act('nudge')} title="Words for the agent, or a veto of its next step">Nudge</Chip> : null}
    {writer && (guard === 'stop' || guard === 'held' || guard === 'waiting')
      ? <Chip tone={guard === 'waiting' ? undefined : 'primary'} onClick={act('approve')}
        title="Reset the run, optionally granting higher limits or lower odds">Approve</Chip> : null}
    {writer && guard !== 'stop'
      ? <Chip onClick={act('kill')} title="Stop the run until it is reset, durably">Kill</Chip> : null}
    {!writer ? <Chip onClick={act('open')}>Tape</Chip> : null}
  </span>;
}

/** What a row opens onto: why the guard says what it says, the foresight in
    a sentence, and the run's nudges. */
function RowDetail({ entry, writer, onAction }) {
  const row = entry.row;
  const nudges = useNudges(row.run, row.tenant || '');
  const reasons = guardState(entry) === 'held' ? (entry.held.doc.reasons || []) : (row.reasons || []);
  return <div data-testid="live-detail" style={{
    padding: '10px 14px 14px 16px', borderBottom: '1px solid var(--hairline)', background: 'var(--bg)',
    display: 'grid', gridTemplateColumns: 'minmax(0, 1fr) minmax(0, 1fr)', gap: 16,
  }} className="tz-reflow-1">
    <div style={{ display: 'grid', gap: 10, alignContent: 'start' }}>
      <Section title="guard">
        <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap', fontSize: 12, color: 'var(--ink-muted)' }}>
          <GuardChip entry={entry} />
          <span>policy <Mono>{row.policy}</Mono></span>
          <span><Mono>{fmtNum(row.errors)}</Mono> errors</span>
          <span>service <Mono>{row.service}</Mono></span>
          <span>started <Mono>{fmtTimeNs(row.started_ns)}</Mono></span>
        </div>
        {reasons.length ? <ul style={{ margin: '6px 0 0', paddingLeft: 18, fontSize: 13, lineHeight: '20px' }}>
          {reasons.map((reason, i) => <li key={i} style={{ color: 'var(--error)' }}><span style={{ color: 'var(--ink)' }}>{reason}</span></li>)}
        </ul> : null}
        {guardState(entry) === 'held' ? <div style={{ fontSize: 12, color: 'var(--ink-muted)', marginTop: 6, lineHeight: '18px' }}>
          Held: the run asked to take its next step, and its verdict for the path so far is stop — a predictive
          limit, which the listing never carries, read from the run's verdict. That read judges the path without the
          step the check was asked about, so a hold on that step alone cannot be read from here.
        </div> : null}
        {guardState(entry) === 'waiting' ? <div style={{ fontSize: 12, color: 'var(--ink-muted)', marginTop: 6, lineHeight: '18px' }}>
          {waitingSentence(row)} Its verdict for the path so far was read and is not stop.
        </div> : null}
      </Section>
      <Section title="foresight">
        <ForesightBar foresight={row.foresight} deferred={Boolean(row.foresight_deferred)} />
        <div style={{ fontSize: 12, color: 'var(--ink-muted)', lineHeight: '18px' }}>{foresightSentence(row.foresight, Boolean(row.foresight_deferred))}</div>
      </Section>
      <div style={{ display: 'flex', gap: 6, flexWrap: 'wrap' }}>
        <Chip onClick={() => onAction('open', entry)}>Open the tape</Chip>
        {writer ? <Chip onClick={() => onAction('nudge', entry)}>Nudge</Chip> : null}
      </div>
    </div>
    <Section title="nudges">
      {nudges.error ? <div style={{ fontSize: 12, color: 'var(--ink-muted)' }}>{nudges.error.what}</div> : null}
      {nudges.data ? <NudgeHistory nudges={nudges.data.nudges} /> : <Skeleton height={40} />}
    </Section>
  </div>;
}

function Section({ title, children }) {
  return <div style={{ display: 'grid', gap: 6, alignContent: 'start', minWidth: 0 }}>
    <div style={{ fontSize: 11, textTransform: 'uppercase', letterSpacing: 'var(--tracking-caps)', color: 'var(--ink-muted)', fontWeight: 500 }}>{title}</div>
    {children}
  </div>;
}

function Cell({ children, align }) {
  return <div role="cell" style={{ padding: 'var(--row-py) 10px', minWidth: 0, textAlign: align || 'left' }}>{children}</div>;
}

function Num({ children, muted, accent }) {
  return <div role="cell" style={{
    padding: 'var(--row-py) 10px', fontFamily: 'var(--font-mono)', fontSize: 12, fontVariantNumeric: 'tabular-nums',
    textAlign: 'right', whiteSpace: 'nowrap', color: accent ? 'var(--accent)' : muted ? 'var(--ink-muted)' : 'var(--ink)',
  }}>{children}</div>;
}

/** One run as a card: the phone layout. */
const LiveCard = React.memo(function LiveCard({ entry, idle, operator, writer, onAction }) {
  const row = entry.row;
  const guard = guardState(entry);
  return <Card data-testid="live-card" data-run={row.run} data-guard={guard} pad="10px 12px" style={{
    position: 'relative', isolation: 'isolate', overflow: 'hidden', opacity: idle ? 0.7 : 1,
    borderLeft: `2px solid ${guard === 'stop' || guard === 'held' ? 'var(--error)' : 'var(--hairline)'}`,
  }}>
    <Flash at={entry.flashAt} />
    <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
      <a href={runHref(row)} onClick={(e) => { e.preventDefault(); onAction('open', entry); }}
        style={{ fontFamily: 'var(--font-mono)', fontSize: 13, minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{row.run}</a>
      <span style={{ marginLeft: 'auto', display: 'inline-flex', gap: 6, alignItems: 'center', flex: 'none' }}>
        <ForesightChip foresight={row.foresight} deferred={Boolean(row.foresight_deferred)} />
        <GuardChip entry={entry} />
      </span>
    </div>
    <div style={{ fontSize: 11, color: 'var(--ink-muted)', marginTop: 2 }}>
      {row.agent}{operator ? ` · ${row.tenant || 'default'}` : ''}
    </div>
    <div style={{ marginTop: 8 }}><StepCell row={row} /></div>
    {row.foresight && row.foresight.verdict !== 'insufficient_history' && row.foresight.p_success != null
      ? <div style={{ marginTop: 8 }}><ForesightBar foresight={row.foresight} compact /></div> : null}
    <div style={{ display: 'flex', gap: 12, flexWrap: 'wrap', marginTop: 8, fontFamily: 'var(--font-mono)', fontSize: 12, fontVariantNumeric: 'tabular-nums', color: 'var(--ink-muted)' }}>
      <span><span style={{ color: 'var(--ink)' }}>{fmtNum(row.steps)}</span> steps</span>
      <span><span style={{ color: 'var(--ink)' }}>{fmtCompact(row.tokens)}</span> tokens</span>
      <span>$<Ticking value={row.cost_usd} format={fmtUsd} style={{ color: 'var(--accent)' }} /></span>
      {row.pending_nudges ? <span style={{ color: 'var(--warn)' }}>{row.pending_nudges} nudge{row.pending_nudges === 1 ? '' : 's'} waiting</span> : null}
    </div>
    {(guard === 'stop' || guard === 'held') && (guard === 'held' ? entry.held.doc.reasons : row.reasons)?.length
      ? <div style={{ marginTop: 6, fontSize: 12, color: 'var(--error)', lineHeight: '17px' }}>
        {(guard === 'held' ? entry.held.doc.reasons : row.reasons)[0]}
      </div> : null}
    <div style={{ marginTop: 10 }}>
      <Actions entry={entry} writer={writer} onAction={onAction} wrap />
      {writer ? <span style={{ marginLeft: 6 }}><Chip onClick={() => onAction('open', entry)}>Tape</Chip></span> : null}
    </div>
  </Card>;
});

function Tile({ label, value, note, tone, active, onClick, title }) {
  const clickable = Boolean(onClick);
  return <div role={clickable ? 'button' : undefined} tabIndex={clickable ? 0 : undefined} title={title}
    aria-pressed={clickable ? Boolean(active) : undefined}
    onClick={onClick} onKeyDown={clickable ? (e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); onClick(); } } : undefined}
    style={{
      padding: '10px 14px', borderRight: '1px solid var(--hairline)', borderBottom: '1px solid var(--hairline)',
      marginBottom: -1, minWidth: 0, cursor: clickable ? 'pointer' : 'default',
      background: active ? 'var(--accent-tint)' : 'transparent',
    }}>
    <div style={{ fontSize: 11, color: 'var(--ink-muted)', marginBottom: 3, whiteSpace: 'nowrap' }}>{label}</div>
    <div style={{ fontFamily: 'var(--font-mono)', fontSize: 18, lineHeight: '24px', fontVariantNumeric: 'tabular-nums', color: tone || 'var(--accent)', whiteSpace: 'nowrap' }}>
      {value}{note ? <span style={{ fontSize: 11, color: 'var(--ink-muted)', marginLeft: 6 }}>{note}</span> : null}
    </div>
  </div>;
}

function StatusLine({ status, updatedAt, breaks }) {
  const dot = status.state === 'live' ? 'var(--ok)' : status.state === 'polling' ? 'var(--warn)'
    : status.state === 'reconnecting' ? 'var(--warn)' : status.state === 'rejected' ? 'var(--error)' : 'var(--ink-faint)';
  let text;
  if (status.state === 'live') text = 'live';
  else if (status.state === 'reconnecting') text = `reconnecting in ${Math.round((status.inMs || 0) / 100) / 10} s`;
  else if (status.state === 'polling') text = 'polling every 5 s';
  else if (status.state === 'connecting') text = 'connecting';
  else if (status.state === 'rejected') text = 'refused';
  else text = status.state;
  const asOf = status.state !== 'live' && updatedAt ? `rows as of ${new Date(updatedAt).toISOString().slice(11, 19)}Z` : null;
  return <span data-testid="live-status" data-state={status.state} role="status" style={{ display: 'inline-flex', alignItems: 'center', gap: 7, fontSize: 12, color: 'var(--ink-muted)', whiteSpace: 'nowrap' }}
    title={status.state === 'polling' && status.error ? `The stream was refused: ${status.error.what}`
      : status.state === 'reconnecting' && status.error ? `The connection ended: ${status.error.what}`
        : breaks ? `${breaks} reconnect${breaks === 1 ? '' : 's'}, each healed by a fresh snapshot` : undefined}>
    <LiveDot color={dot} />
    {text}
    {asOf ? <><span style={{ color: 'var(--ink-faint)' }}>·</span><span>{asOf}</span></> : null}
  </span>;
}

export function LiveScreen({ go, params, identity, pushToast }) {
  const operator = Boolean(identity && identity.operator);
  const writer = canWrite(identity);
  const narrow = useMedia('(max-width: 699px)');
  const tenantFilter = operator ? (params.get('tenant') || '') : '';
  const [tenantDraft, setTenantDraft] = React.useState(tenantFilter);
  const filter = FILTERS.some((f) => f.id === params.get('filter')) ? params.get('filter') : 'all';
  const sort = SORTS.some((s) => s.id === params.get('sort')) ? params.get('sort') : 'risk';
  const agent = params.get('agent') || '';
  const [search, setSearch] = React.useState('');
  const [open, setOpen] = React.useState(() => new Set());
  const [nudging, setNudging] = React.useState(null);
  const [tripping, setTripping] = React.useState(null);
  const [resetting, setResetting] = React.useState(null);

  const live = useLiveRuns(tenantFilter);
  const now = useNow(1000);
  const serverNow = serverNowMs(live.state, now);
  const { held, forget } = useHeldProbe(live.state, serverNow);
  // Which runs are waiting changes rarely; recomputing the entries only then
  // keeps the rows' memo good while the clock ticks.
  const waiting = waitingKeys(live.state.rows, serverNow);
  const approveAsk = React.useRef(0);

  // A tenant is typed, not clicked: it waits for typing to settle before the
  // stream is re-opened under it.
  React.useEffect(() => {
    if (tenantDraft === tenantFilter) return undefined;
    const timer = setTimeout(() => setParam('tenant', tenantDraft.trim()), 400);
    return () => clearTimeout(timer);
  }, [tenantDraft]); // eslint-disable-line react-hooks/exhaustive-deps

  const setParam = (key, value) => {
    const next = new URLSearchParams(params);
    if (value) next.set(key, value); else next.delete(key);
    navigate(['live'], Object.fromEntries(next), { replace: true });
  };

  const entries = React.useMemo(() => {
    const waits = new Set(waiting ? waiting.split('\u0001') : []);
    return [...live.state.rows.values()].map((entry) => {
      const key = rowKey(entry.row);
      const hold = held.get(key);
      const isHeld = hold && hold.activity === entry.row.last_activity_ns;
      if (!isHeld && !waits.has(key)) return entry;
      return { ...entry, ...(isHeld ? { held: hold } : {}), ...(waits.has(key) ? { waiting: true } : {}) };
    });
  }, [live.state.rows, held, waiting]);
  const totals = liveTotals(entries, serverNow);
  // The snapshot was cut, or the page let runs go: every figure is of the
  // runs held, and says so.
  const partial = isPartial(live.state);
  const evicted = live.state.evicted ? live.state.evicted.size : 0;
  const agents = agentsOf(entries);
  const shown = sortEntries(entries.filter((entry) => matches(entry, { filter, agent, search }, serverNow)), sort, serverNow);

  const onToggle = React.useCallback((key) => setOpen((set) => {
    const next = new Set(set);
    if (next.has(key)) next.delete(key); else next.add(key);
    return next;
  }), []);

  const onAction = React.useCallback((name, entry) => {
    const row = entry.row;
    if (name === 'open') go(['run', row.run], tenantParams(row.tenant || ''));
    else if (name === 'nudge') setNudging({ run: row.run, tenant: row.tenant || '', foresight: row.foresight, current: row.current });
    else if (name === 'kill') setTripping({ run: row.run, tenant: row.tenant || '' });
    else if (name === 'approve') {
      // The reset dialog shows the limits in force as placeholders; the live
      // row does not carry them, the verdict does. A held run's verdict is
      // already in hand. Only the latest click's answer opens the dialog:
      // two verdicts in flight can land in either order.
      const ask = ++approveAsk.current;
      const waitingNote = !entry.held && entry.waiting ? waitingSentence(row) : null;
      if (entry.held) { setResetting({ doc: entry.held.doc, note: null }); return; }
      api.fuseRun(row.run, tenantParams(row.tenant || '')).then(
        (doc) => { if (ask === approveAsk.current) setResetting({ doc, note: waitingNote }); },
        () => { if (ask === approveAsk.current) setResetting({ doc: { run: row.run, tenant: row.tenant || '', reasons: row.reasons }, note: waitingNote }); },
      );
    }
  }, [go]);

  const afterControl = (doc, verb, target) => {
    // What the probe knew about the run is out of date: a reset may have
    // lifted the hold, a trip replaced it with the guard's own stop.
    if (target) forget(rowKey({ run: target.run, tenant: target.tenant || '' }));
    if (doc && doc.verdict === 'stop' && verb === 'reset') {
      pushToast({
        status: 'warn', title: 'Reset, and stopped again',
        detail: (doc.reasons || []).join('; ') + '. Grant a higher limit (or lower odds) to let it continue.',
      });
    } else {
      pushToast({ status: 'ok', title: verb === 'trip' ? 'Kill switch on' : 'Approved to continue', detail: `${doc.run}: ${doc.verdict}` });
    }
  };

  if (live.status.state === 'disabled') {
    return <EmptyState message={<span>Live runs need run fuses, which are off on this server: it was
      started with <code>--no-fuses</code>, so it follows no runs and every live-run and nudge route
      answers <code>409</code>. Restart without the flag to steer runs here.</span>} />;
  }
  if (live.status.state === 'missing') {
    return <EmptyState message={<span>This server has no live-run view: <code>GET /v1/runs/live/stream</code>
      answered <code>404</code>, so it predates Traza 0.28. The Guard screen lists the runs it follows.</span>} />;
  }
  if (live.status.state === 'denied') {
    return <ErrorState what={live.status.error?.what || 'The live view was refused.'} next={live.status.error?.next} />;
  }

  // Nothing is said about the runs until a listing has been read in this
  // scope: before that, "no run is live" would be a guess.
  const rejected = live.status.state === 'rejected';
  const connected = live.updatedAt != null && !rejected;
  const lastFailure = live.status.state === 'reconnecting' ? live.status.error
    : live.status.state === 'polling' ? live.status.failed : null;

  return <ClockContext.Provider value={serverNow}>
    <div style={{ display: 'grid', gap: 12, maxWidth: 1700 }}>
      <Card pad="0" className="tz-reflow" style={{ display: 'grid', gridTemplateColumns: 'repeat(6, minmax(0, 1fr))', overflow: 'hidden' }}>
        <Tile label="live runs" value={fmtNum(totals.live) + (partial ? '+' : '')} note={`${fmtNum(totals.active)} active`} active={filter === 'all'}
          onClick={() => setParam('filter', '')} title={'Runs with activity in the live window; active ones had some in the last 30 seconds'
            + (partial ? '. More are live than this page holds: every figure here counts the runs it holds.' : '')} />
        <Tile label="spend, live runs" value={<>$<Ticking value={totals.cost} format={fmtUsd} /></>}
          title="The guard's cost for every run listed, summed" />
        <Tile label="tokens" value={fmtCompact(totals.tokens)} tone="var(--ink)" />
        <Tile label="need attention" value={fmtNum(totals.attention)} tone={totals.attention ? 'var(--warn)' : 'var(--ink)'}
          active={filter === 'attention'} onClick={() => setParam('filter', 'attention')}
          title="Stopped or held, likely to fail or at risk by foresight, warned by the guard, or on a novel path" />
        <Tile label="stopped or held" value={fmtNum(totals.stopped)} tone={totals.stopped ? 'var(--error)' : 'var(--ink)'}
          active={filter === 'stopped'} onClick={() => setParam('filter', 'stopped')} />
        <Tile label="nudges waiting" value={fmtNum(totals.nudges)} tone="var(--ink)"
          active={filter === 'nudged'} onClick={() => setParam('filter', 'nudged')} />
      </Card>

      <div style={{ display: 'flex', alignItems: 'center', gap: 6, flexWrap: 'wrap' }}>
        {FILTERS.map((f) => <Chip key={f.id} active={filter === f.id} onClick={() => setParam('filter', f.id === 'all' ? '' : f.id)}>{f.label}</Chip>)}
        <span style={{ width: 6 }} />
        <span style={{ fontSize: 12, color: 'var(--ink-faint)' }}>sort</span>
        {SORTS.map((s) => <Chip key={s.id} mono active={sort === s.id} onClick={() => setParam('sort', s.id === 'risk' ? '' : s.id)}>{s.label}</Chip>)}
        {agents.length > 1 ? <select aria-label="Filter by agent" value={agent} onChange={(e) => setParam('agent', e.target.value)}
          style={{ font: 'inherit', fontSize: 12, padding: '3px 6px', border: '1px solid var(--hairline)', borderRadius: 'var(--radius-control)', background: 'var(--bg-raised)', color: 'var(--ink)' }}>
          <option value="">every agent</option>
          {agents.map((a) => <option key={a.agent} value={a.agent}>{a.agent} ({a.runs})</option>)}
        </select> : null}
        <Input size="sm" mono placeholder="find a run" ariaLabel="Find a run" value={search} onChange={setSearch} style={{ width: 150 }} />
        {operator ? <Input size="sm" mono placeholder="tenant (all)" ariaLabel="Filter by tenant" value={tenantDraft}
          onChange={setTenantDraft} style={{ width: 130 }} /> : null}
        <span style={{ marginLeft: 'auto' }}><StatusLine status={live.status} updatedAt={live.updatedAt} breaks={live.breaks} /></span>
      </div>

      {live.note ? <div role="status" style={{ fontSize: 12, color: 'var(--ink-muted)' }}>
        {live.note.kind === 'rebuilt'
          ? 'Reconnected. The view was rebuilt from a fresh snapshot: the stream has no resume cursor, so nothing from the break is patched in.'
          : 'The server resent the whole view: more than 256 runs changed at once, or an erasure swept it.'}
      </div> : null}
      {live.status.state === 'polling' ? <div style={{ fontSize: 12, color: 'var(--warn)' }}>
        The stream was refused ({live.status.error?.what || 'too many open'}), so the listing is read every five
        seconds instead; the stream is tried again in half a minute.
      </div> : null}
      {live.state.truncated ? <div style={{ fontSize: 12, color: 'var(--ink-muted)' }}>
        More runs are live than the snapshot holds (256, the most recently changed), so every figure above counts
        only the runs listed.{operator ? ' Filter by tenant to narrow it.' : ''}
      </div> : null}
      {evicted ? <div data-testid="live-partial" style={{ fontSize: 12, color: 'var(--warn)' }}>
        This page keeps {fmtNum(ROW_CAP)} runs, the most recently active, and has let {fmtNum(evicted)} more
        go since the last snapshot: every figure above leaves {evicted === 1 ? 'it' : 'them'} out until
        {evicted === 1 ? ' it leaves' : ' they leave'} the live view or a fresh snapshot covers every run.
      </div> : null}

      {rejected ? <ErrorState what={live.status.error?.what || 'The server refused the live view.'}
        next={tenantFilter ? 'Change the tenant above: the server refused the view under it.' : live.status.error?.next}
        onRetry={live.retry} /> : null}
      {!connected && !rejected && lastFailure ? <ErrorState what={`Not connected yet: ${lastFailure.what || 'the server did not answer.'}`}
        next={'No listing has been read in this scope yet, so this page cannot say whether any run is live. '
          + (live.status.state === 'reconnecting' ? `Trying again in ${Math.round((live.status.inMs || 0) / 100) / 10} s.` : 'Reading again every five seconds.')} /> : null}
      {!connected && !rejected && !lastFailure ? <Skeleton height={180} /> : null}
      {connected && !entries.length ? <EmptyState message={<span>
        No run has had activity in the live window (15 minutes by default, <code>--live-window-seconds</code>). A run
        is live while its spans arrive, or while its agent asks the pre-call check before each step — the SDK
        guards and the Claude Code hook pack do — and it appears here within half a second.
      </span>} /> : null}
      {connected && entries.length && !shown.length ? <EmptyState message="No live run matches this filter." /> : null}

      {connected && shown.length ? (narrow
        ? <div style={{ display: 'grid', gap: 8 }} aria-label="Live runs">
          {shown.map((entry) => <LiveCard key={rowKey(entry.row)} entry={entry} idle={isQuiet(entry.row, serverNow)}
            operator={operator} writer={writer} onAction={onAction} />)}
        </div>
        : <Card pad="0" style={{ overflow: 'hidden' }}>
          <div className="tz-scroll-x">
            <div style={{ minWidth: 1140 }} role="table" aria-label="Live runs">
              <div role="row" style={{ display: 'grid', gridTemplateColumns: COLUMNS, background: 'var(--bg-sunken)', borderBottom: '1px solid var(--hairline)' }}>
                {HEADERS.map((label, i) => <div key={i} role="columnheader" style={{
                  padding: '6px 10px', fontSize: 12, fontWeight: 500, color: 'var(--ink-muted)', whiteSpace: 'nowrap',
                  textAlign: i >= 2 && i <= 4 ? 'right' : i >= 7 ? 'right' : 'left',
                }}>{label}</div>)}
              </div>
              {shown.map((entry) => {
                const key = rowKey(entry.row);
                return <LiveRow key={key} entry={entry} idle={isQuiet(entry.row, serverNow)} open={open.has(key)}
                  operator={operator} writer={writer} onToggle={onToggle} onAction={onAction} />;
              })}
            </div>
          </div>
        </Card>) : null}

      {entries.length ? <div style={{ fontSize: 12, color: 'var(--ink-faint)', lineHeight: '18px' }}>
        From the guard's memory, as the pre-call check sees it: a run is listed with what this process has seen of
        it. Foresight counts how earlier runs of the same agent that took the same path ended — at 30 runs of
        support — and is never a model's guess. {writer ? '' : 'Nudges, the kill switch and approvals need an rw credential.'}
      </div> : null}
    </div>

    <NudgeModal target={nudging} onClose={() => setNudging(null)} />
    <TripModal run={tripping} onClose={() => setTripping(null)}
      onDone={(doc) => { const target = tripping; setTripping(null); afterControl(doc, 'trip', target); }} />
    <ResetModal run={resetting ? resetting.doc : null} note={resetting ? resetting.note : null} onClose={() => setResetting(null)}
      onDone={(doc) => { const target = resetting && resetting.doc; setResetting(null); afterControl(doc, 'reset', target); }} />
  </ClockContext.Provider>;
}
