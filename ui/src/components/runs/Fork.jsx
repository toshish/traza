import React from 'react';
import { api } from '../../lib/api.js';
import { publicBase } from '../../lib/base.js';
import { useRead } from '../../lib/route.js';
import { fmtDurationNs, fmtNum, fmtTimeNs } from '../../lib/format.js';
import { fmtMoney, tenantParams } from '../../lib/runs.js';
import {
  buildForkRequest, editValue, forkCli, forkEnv, forkPython, forkSummary, recordedAnswer,
} from '../../lib/fork.js';
import { Card, Chip, ErrorState, Mono, PanelHead } from '../primitives/Chrome.jsx';
import { Tag } from '../primitives/Tag.jsx';
import { Modal } from '../primitives/Modal.jsx';
import { Button } from '../primitives/Button.jsx';
import { CodeBlock } from '../data/CodeBlock.jsx';

// Fork & Rewind (docs/guide/fork.md): serve a different answer at one step
// of a recorded run and let the agent carry on, for real, down a branch the
// recording never took — or replay a long run's beginning for free and go
// live from any step. Traza never calls a model: the fork's live half is the
// agent's own traffic, from the agent's own process.

const OUTCOME_TAG = { success: 'ok', failure: 'error', abandoned: 'warn', unknown: 'neutral' };

/** The editor, opened on one tape step. `step` is the tape step, or null
    when closed; `onCreated(replay)` hears about a fork the server made. */
export function ForkModal({ run, tenant, step, identity, onClose, onCreated }) {
  const open = Boolean(step);
  const kind = step?.kind;
  const [change, setChange] = React.useState('edit');
  const [format, setFormat] = React.useState('text');
  const [text, setText] = React.useState('');
  const [live, setLive] = React.useState(true);
  const [mode, setMode] = React.useState('strict');
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState(null);
  const [invalid, setInvalid] = React.useState('');
  const [created, setCreated] = React.useState(null);
  const [status, setStatus] = React.useState(null);
  const recorded = React.useMemo(() => (step ? recordedAnswer(step) : null), [step]);

  React.useEffect(() => {
    if (!step) return;
    setChange('edit');
    setFormat(recorded.format);
    setText(recorded.text);
    setLive(true);
    setMode('strict');
    setBusy(false); setError(null); setInvalid(''); setCreated(null); setStatus(null);
  }, [step, recorded]);

  if (!open) return null;
  const index = step.index;
  const name = step.model || step.tool || step.agent || step.name;
  const request = { run, tenant, step: index, change, kind, format, text, live, mode };

  const create = async () => {
    const built = buildForkRequest(request);
    if (built.error) { setInvalid(built.error); return; }
    setInvalid('');
    setBusy(true);
    setError(null);
    try {
      const replay = await api.createReplay(built.body);
      // The value the snippets repeat is the one the server accepted.
      setCreated({ replay, request: { ...request, value: change === 'edit' ? editValue(request).value : undefined } });
      onCreated?.(replay);
    } catch (e) {
      setError(e);
    } finally { setBusy(false); }
  };

  const check = async () => {
    try { setStatus(await api.replay(created.replay.id)); } catch (e) { setError(e); }
  };

  return <Modal open title={`Fork ${run} at step ${index}`} width={720} onClose={onClose} footer={created ? <>
    <Button onClick={() => { setCreated(null); setStatus(null); }}>Edit again</Button>
    <Button variant="primary" onClick={onClose}>Done</Button>
  </> : <>
    <Button onClick={onClose}>Cancel</Button>
    <Button variant="primary" onClick={create} disabled={busy}>{busy ? 'Creating…' : 'Create fork'}</Button>
  </>}>
    {created ? <ForkCreated created={created} identity={identity} status={status} onCheck={check} error={error} />
      : <div style={{ display: 'grid', gap: 12 }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap', fontSize: 12, color: 'var(--ink-muted)' }}>
          <Tag mono>{kind}</Tag>
          <Mono color="var(--ink)">{name}</Mono>
          <span>step {index} · {step.status}</span>
        </div>

        <div role="group" aria-label="What changes" style={{ display: 'grid', gap: 6 }}>
          <Choice active={change === 'edit'} onPick={() => setChange('edit')}
            title={kind === 'llm' ? 'Serve a different model answer here' : 'Serve a different tool result here'}
            detail={`Steps before ${index} are replayed from the recording; step ${index} gets your ${kind === 'llm' ? 'answer' : 'result'} instead.`} />
          <Choice active={change === 'rewind'} disabled={index < 1} onPick={() => index >= 1 && setChange('rewind')}
            title={`Rewind: go live at step ${index}`}
            detail={index < 1 ? 'The first step has nothing before it to replay.'
              : `Steps 0–${index - 1} are replayed exactly, at no model cost; from step ${index} on, every call goes to the real provider.`} />
        </div>

        {change === 'edit' ? <div style={{ display: 'grid', gap: 6 }}>
          <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
            <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>{kind === 'llm' ? 'answer' : 'tool result'}, as</span>
            <Chip mono active={format === 'text'} onClick={() => setFormat('text')}>text</Chip>
            <Chip mono active={format === 'json'} onClick={() => setFormat('json')}>JSON</Chip>
            {recorded.offloaded ? <span style={{ fontSize: 11, color: 'var(--warn)' }}>the recorded answer is offloaded; inline payloads to start from it</span> : null}
          </div>
          <span className="tz-field" style={{ display: 'block', border: `1px solid ${invalid ? 'var(--error)' : 'var(--hairline)'}`, borderRadius: 'var(--radius-control)', background: 'var(--bg-raised)' }}>
            <textarea value={text} onChange={(e) => { setText(e.target.value); setInvalid(''); }} rows={9} spellCheck={format === 'text'}
              aria-label={kind === 'llm' ? 'Edited answer' : 'Edited tool result'} aria-invalid={Boolean(invalid)}
              style={{
                display: 'block', width: '100%', resize: 'vertical', border: 'none', outline: 'none', background: 'transparent',
                color: 'var(--ink)', fontFamily: format === 'json' ? 'var(--font-mono)' : 'var(--font-sans)', fontSize: format === 'json' ? 12 : 13,
                lineHeight: '19px', padding: '8px 10px', minHeight: 150,
              }} />
          </span>
          <div style={{ fontSize: 11, color: 'var(--ink-faint)', lineHeight: '16px' }}>
            {kind === 'llm'
              ? 'Text, or as JSON an assistant message in OpenAI, Anthropic or normalised shape (tool calls included), a list of them, or a response body with choices. It is served under the recording\'s model name, with an invented response id and zero usage.'
              : 'Any JSON, null included, served as the step\'s result with "edited": true; the arguments stay the recording\'s. As text, it is served as a JSON string.'}
          </div>
          <label style={{ display: 'flex', alignItems: 'flex-start', gap: 8, fontSize: 12, color: 'var(--ink)', cursor: 'pointer', marginTop: 4 }}>
            <input type="checkbox" checked={live} onChange={(e) => setLive(e.target.checked)} style={{ marginTop: 3, accentColor: 'var(--accent)' }} />
            <span>
              Go live after this step
              <span style={{ display: 'block', color: 'var(--ink-muted)', fontSize: 11 }}>
                {live ? `Past step ${index} every model call and tools/next answers 409 replay_live, and the harness sends it to the real provider.`
                  : 'The recording plays on after the edit, loosely: it exercises the agent\'s own code down the recorded path, not what a model would have said.'}
              </span>
            </span>
          </label>
        </div> : null}

        <div style={{ display: 'flex', alignItems: 'center', gap: 6, flexWrap: 'wrap' }}>
          <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>matching before the cut</span>
          <Chip mono active={mode === 'strict'} onClick={() => setMode('strict')} title="Refuse a request whose messages differ from the recording, up to and including the first edited step">strict</Chip>
          <Chip mono active={mode === 'loose'} onClick={() => setMode('loose')} title="Serve the recorded answers in order, whatever is asked">loose</Chip>
        </div>

        {invalid ? <div role="alert" style={{ fontSize: 12, color: 'var(--error)' }}>{invalid}</div> : null}
        {error ? <ErrorState what={error.what || 'The fork was not created.'} next={error.next || (error.status === 409
          ? 'This run cannot be replayed as recorded up to the cut; the message names the step. A rewind to an earlier step, or loose matching, may still work.'
          : undefined)} /> : null}
      </div>}
  </Modal>;
}

function Choice({ active, disabled, onPick, title, detail }) {
  return <button type="button" className="tz-plain-button" onClick={onPick} disabled={disabled} aria-pressed={active}
    style={{
      display: 'grid', gap: 2, padding: '8px 10px', borderRadius: 'var(--radius-card)', cursor: disabled ? 'not-allowed' : 'pointer',
      border: `1px solid ${active ? 'var(--accent)' : 'var(--hairline)'}`, background: active ? 'var(--accent-tint)' : 'var(--bg-raised)',
      opacity: disabled ? 0.5 : 1, width: '100%',
    }}>
    <span style={{ fontSize: 13, fontWeight: 500, color: 'var(--ink)' }}>{title}</span>
    <span style={{ fontSize: 12, color: 'var(--ink-muted)', lineHeight: '17px' }}>{detail}</span>
  </button>;
}

/** What the server made: the fork's id and cut, the environment for a
    harness of your own, and the same fork from the SDK and the CLI. */
function ForkCreated({ created, identity, status, onCheck, error }) {
  const { replay, request } = created;
  const fork = replay.fork || {};
  return <div data-testid="fork-created" style={{ display: 'grid', gap: 12 }}>
    <div style={{ display: 'flex', gap: 12, flexWrap: 'wrap', alignItems: 'baseline', fontSize: 12, color: 'var(--ink-muted)' }}>
      <span>fork <Mono color="var(--ink)">{replay.fork_id}</Mono></span>
      <span>cut at step <Mono color="var(--accent)">{fork.step}</Mono></span>
      <span>{(fork.edits || []).length ? `edits ${(fork.edits || []).map((e) => `${e.kind} at ${e.step}`).join(', ')}` : 'no edits'}</span>
      <span>{fork.live_after != null ? <>live after step <Mono>{fork.live_after}</Mono></> : 'recording plays on'}</span>
      <span><Mono>{replay.llm_steps}</Mono> model answers held · expires <Mono>{fmtTimeNs(replay.expires_at_ns)}</Mono></span>
    </div>
    {replay.steps_truncated ? <div style={{ fontSize: 12, color: 'var(--warn)' }}>
      The run is longer than a replay holds; the live point falls within the steps read, so the fork was accepted.
    </div> : null}
    <div>
      <div style={{ fontSize: 12, color: 'var(--ink-muted)', marginBottom: 4 }}>
        A harness of your own: point the SDK at the replay, give the forked run its own session id, and tag its spans.
      </div>
      <CodeBlock language="sh" code={forkEnv(replay, publicBase(), identity?.auth)} />
    </div>
    <div>
      <div style={{ fontSize: 12, color: 'var(--ink-muted)', marginBottom: 4 }}>
        Or let the Python client do the switching and the tagging — it makes a fork of its own with the same edit:
      </div>
      <CodeBlock language="python" code={forkPython(request)} />
      <div style={{ marginTop: 6 }}><CodeBlock language="sh" code={forkCli(request)} /></div>
    </div>
    <div style={{ display: 'flex', gap: 8, alignItems: 'center', flexWrap: 'wrap', fontSize: 12, color: 'var(--ink-muted)' }}>
      <Chip onClick={onCheck}>What has it served?</Chip>
      {status ? <span data-testid="fork-status">
        served <Mono color="var(--accent)">{status.served}</Mono>
        {status.fork?.live_at_ns ? <> · went live at <Mono>{fmtTimeNs(status.fork.live_at_ns)}</Mono>, <Mono>{status.fork.live_calls}</Mono> live calls</> : ' · not live yet'}
        {(status.fork?.edits || []).map((e) => <span key={e.step}> · {e.kind} at {e.step} {e.served ? 'served' : 'not served yet'}</span>)}
        {status.diverged ? <span style={{ color: 'var(--error)' }}> · diverged at step {status.diverged.step}</span> : null}
      </span> : <span>The fork is listed under Forks once its spans arrive.</span>}
    </div>
    {error ? <ErrorState what={error.what} next={error.next} /> : null}
  </div>;
}

/** The run's forks, each compared with it: where it parted, how it ended,
    what it cost — and the diff a click away. */
export function ForksCard({ run, tenant, go, nonce = 0 }) {
  const read = useRead((signal) => api.forks({ run, ...tenantParams(tenant) }, signal), [run, tenant, nonce]);
  if (read.error && read.error.status !== 404) {
    return <Card><PanelHead title="Forks" /><ErrorState what={read.error.what} next={read.error.next} onRetry={read.reload} /></Card>;
  }
  const doc = read.data;
  const forks = (doc?.forks || []).map((fork) => forkSummary(fork, doc.parent));
  return <Card data-testid="forks-card">
    <PanelHead title="Forks" note="runs whose spans carry traza.fork.parent = this run"
      action={<Chip onClick={read.reload}>Refresh</Chip>} />
    {doc && !forks.length ? <div style={{ fontSize: 12, color: 'var(--ink-muted)', lineHeight: '18px' }}>
      No forks yet. Open a step on the tape and choose <em>Fork from here</em>.
    </div> : null}
    <div style={{ display: 'grid', gap: 8 }}>
      {forks.map((fork) => <div key={fork.tenant + '\u0000' + fork.run} data-testid="fork-row" style={{
        border: '1px solid var(--hairline)', borderRadius: 'var(--radius-card)', padding: '8px 10px', display: 'grid', gap: 5,
      }}>
        <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
          <a href={'#/run/' + encodeURIComponent(fork.run)} title={fork.run} onClick={(e) => { e.preventDefault(); go(['run', fork.run], tenantParams(fork.tenant)); }}
            style={{ fontFamily: 'var(--font-mono)', fontSize: 12, maxWidth: 180, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', display: 'inline-block' }}>{fork.run}</a>
          {fork.step != null ? <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>cut at step <Mono>{fork.step}</Mono></span> : null}
          {fork.outcome ? <Tag status={OUTCOME_TAG[fork.outcome] || 'neutral'} mono>{fork.outcome}</Tag> : null}
          {fork.diffed ? <a href="#/diff" onClick={(e) => { e.preventDefault(); go(['diff'], { a: run, b: fork.run, ...tenantParams(fork.tenant) }); }}
            style={{ marginLeft: 'auto', fontSize: 12 }}>diff against the parent</a> : null}
        </div>
        {fork.diffed ? <div style={{ display: 'flex', gap: 12, flexWrap: 'wrap', fontSize: 12, color: 'var(--ink-muted)' }}>
          <span><Mono>{fmtNum(fork.steps)}</Mono> steps {delta(fork.delta?.steps, fmtNum)}</span>
          <span>$<Mono color="var(--accent)">{fmtMoney(fork.cost)}</Mono> {delta(fork.delta?.cost_usd, fmtMoney)}</span>
          {fork.delta?.duration_ns != null ? <span>duration {delta(fork.delta.duration_ns, (v) => fmtDurationNs(v))}</span> : null}
          {fork.parentOutcome ? <span>parent {fork.parentOutcome}</span> : null}
        </div> : <div style={{ fontSize: 12, color: 'var(--warn)' }}>
          Not compared: {fork.skipped === 'parent_missing' ? 'the parent has no spans in the fork\'s tenant' : 'the query budget ran out before it'}.
        </div>}
        {fork.divergence ? <div style={{ fontSize: 12, color: 'var(--ink)', lineHeight: '18px', overflowWrap: 'anywhere' }}>
          {fork.beforeCut ? <Tag status="warn" mono style={{ marginRight: 6 }}>parted before the cut</Tag> : null}
          {fork.divergence.explanation}
        </div> : null}
      </div>)}
    </div>
    {doc?.merged_into_parent ? <div style={{ fontSize: 12, color: 'var(--warn)', marginTop: 8 }}>
      {fmtNum(doc.merged_into_parent)} tagged spans joined this run itself: a fork whose harness gave it no session id of its own.
    </div> : null}
    {doc?.truncated ? <div style={{ fontSize: 12, color: 'var(--ink-muted)', marginTop: 8 }}>
      The listing is partial: more forks than listed, more than 10,000 tagged spans, or a fork the query budget did not reach.
    </div> : null}
  </Card>;
}

/** A signed change against the parent, "(+2)" or "(−0.004)", in ink. */
function delta(value, fmt) {
  if (typeof value !== 'number' || !Number.isFinite(value) || value === 0) return null;
  const sign = value > 0 ? '+' : '−';
  return <Mono color="var(--ink-muted)">({sign}{fmt(Math.abs(value))})</Mono>;
}
