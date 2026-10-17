import React from 'react';
import { api } from '../lib/api.js';
import { useRead } from '../lib/route.js';
import { fmtDurationNs, fmtNum } from '../lib/format.js';
import { fmtMoney, splitFingerprint } from '../lib/runs.js';
import { Card, Chip, EmptyState, ErrorState, Mono, PanelHead, Skeleton } from '../components/primitives/Chrome.jsx';
import { Tag } from '../components/primitives/Tag.jsx';
import { Input } from '../components/primitives/Input.jsx';

// Two runs aligned step by step, and the first place they parted. The server
// does the alignment (a longest common subsequence over `kind:name`
// signatures) and the classification; this screen draws it, puts the first
// divergence where the eye lands, and links every step back to its tape.

/** What each kind of divergence means, in the guide's words. */
export const WHY = {
  input: 'the same step received different input',
  decision: 'the same input produced a different output',
  config: 'the model or its request parameters changed',
  structure: 'a step only one run has',
};

const OP_TAG = { same: 'neutral', changed: 'warn', insert: 'accent', delete: 'accent' };

export function DiffScreen({ go, params }) {
  const a = params.get('a') || '';
  const b = params.get('b') || '';
  const tenant = params.get('tenant') || '';
  if (!a || !b) return <DiffForm a={a} b={b} tenant={tenant} go={go} />;
  return <DiffView a={a} b={b} tenant={tenant} go={go} />;
}

function DiffForm({ a, b, tenant, go }) {
  const [left, setLeft] = React.useState(a);
  const [right, setRight] = React.useState(b);
  const submit = () => {
    if (!left.trim() || !right.trim()) return;
    go(['diff'], { a: left.trim(), b: right.trim(), ...(tenant ? { tenant } : {}) });
  };
  return <div style={{ display: 'grid', gap: 14, maxWidth: 900 }}>
    <Card>
      <PanelHead title="Compare two runs" />
      <div style={{ fontSize: 13, color: 'var(--ink-muted)', lineHeight: '20px', marginBottom: 12 }}>
        A run is a session id, or <code>trace:&lt;trace_id&gt;</code> for a trace without one. The
        diff aligns their steps and names the first place they differ: an input, a decision, a
        configuration change, or a step only one of them took.
      </div>
      <div className="tz-reflow-1" style={{ display: 'grid', gridTemplateColumns: '1fr 1fr auto', gap: 8, alignItems: 'center' }}>
        <Input mono value={left} onChange={setLeft} placeholder="run a (the reference)" ariaLabel="Run a"
          onKeyDown={(e) => { if (e.key === 'Enter') submit(); }} />
        <Input mono value={right} onChange={setRight} placeholder="run b" ariaLabel="Run b"
          onKeyDown={(e) => { if (e.key === 'Enter') submit(); }} />
        <Chip tone="primary" onClick={submit}>Compare</Chip>
      </div>
    </Card>
  </div>;
}

function DiffView({ a, b, tenant, go }) {
  const diff = useRead((signal) => api.diff({ a, b, tenant: tenant || undefined }, signal), [a, b, tenant]);
  const firstRef = React.useRef(null);
  const scope = tenant ? { tenant } : {};

  if (diff.loading && !diff.data) return <Skeleton height={240} />;
  if (diff.error) {
    return <div style={{ display: 'grid', gap: 12, maxWidth: 900 }}>
      <ErrorState what={diff.error.what}
        next={diff.error.status === 404 ? 'Check the run id: a session id, or trace:<trace_id>.' : diff.error.next}
        onRetry={diff.reload} />
      <DiffForm a={a} b={b} tenant={tenant} go={go} />
    </div>;
  }
  const data = diff.data;
  if (!data) return null;
  const first = data.first_divergence;
  const firstIndex = first ? data.aligned.findIndex((row) => row.op !== 'same') : -1;
  const openStep = (run, step) => go(['run', run], { ...scope, step: String(step) });

  return <div style={{ display: 'grid', gap: 14, maxWidth: 1500 }}>
    <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
      <Chip onClick={() => go(['run', a], scope)}><Mono>a</Mono> {a}</Chip>
      <Chip onClick={() => go(['run', b], scope)}><Mono>b</Mono> {b}</Chip>
      <span style={{ marginLeft: 'auto', display: 'flex', flexWrap: 'wrap', gap: 6 }}>
        <Chip onClick={() => go(['diff'], { a: b, b: a, ...scope })}>Swap</Chip>
        <Chip onClick={() => go(['diff'], scope)}>Other runs</Chip>
      </span>
    </div>

    <Totals data={data} />

    {first ? <Card data-testid="first-divergence" style={{ borderLeft: '2px solid var(--accent)' }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap', marginBottom: 6 }}>
        <span style={{ fontSize: 11, textTransform: 'uppercase', letterSpacing: 'var(--tracking-caps)', color: 'var(--ink-muted)', fontWeight: 500 }}>first divergence</span>
        <Tag status="warn" mono>{first.kind}</Tag>
        <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>{WHY[first.kind]}</span>
        <span style={{ marginLeft: 'auto', display: 'flex', flexWrap: 'wrap', gap: 6 }}>
          <Chip onClick={() => openStep(a, first.a)}>a step {first.a}</Chip>
          <Chip onClick={() => openStep(b, first.b)}>b step {first.b}</Chip>
          {firstIndex >= 0 ? <Chip onClick={() => firstRef.current?.scrollIntoView?.({ block: 'center' })}>Show in table</Chip> : null}
        </span>
      </div>
      <div style={{ fontSize: 13, lineHeight: '20px', color: 'var(--ink)' }}>{first.explanation}</div>
    </Card> : <Card><div style={{ fontSize: 13, color: 'var(--ink)' }}>
      These runs took the same steps with the same inputs, outputs and configuration. Step status
      is not compared — read the error counts above for that.
    </div></Card>}

    {data.truncated ? <div style={{ fontSize: 12, color: 'var(--warn)' }}>
      A side had more than 5,000 steps or 50,000 spans; the alignment describes prefixes, and its tail
      may report steps as missing that the cut removed.
    </div> : null}

    <Card pad="0" style={{ overflow: 'hidden' }}>
      <div style={{ padding: '12px 14px 0' }}>
        <PanelHead title="Aligned steps" note="longest common subsequence over kind:name; the hash is the input digest" />
      </div>
      {data.aligned.length ? <div className="tz-scroll-x">
        <div style={{ minWidth: 760 }} role="table" aria-label="Aligned steps">
          <div role="row" style={{
            display: 'grid', gridTemplateColumns: COLUMNS, background: 'var(--bg-sunken)',
            borderTop: '1px solid var(--hairline)', borderBottom: '1px solid var(--hairline)',
          }}>
            {['op', 'a', 'step in a', 'b', 'step in b', 'why'].map((label) => <div key={label} role="columnheader" style={{
              padding: '6px 10px', fontSize: 12, fontWeight: 500, color: 'var(--ink-muted)',
            }}>{label}</div>)}
          </div>
          {data.aligned.map((row, i) => <AlignedRow key={i} row={row} first={i === firstIndex}
            rowRef={i === firstIndex ? firstRef : undefined}
            onA={() => row.a != null && openStep(a, row.a)} onB={() => row.b != null && openStep(b, row.b)} />)}
        </div>
      </div> : <div style={{ padding: '0 14px 14px' }}>
        <EmptyState message="Neither run has a step: a model call, a tool call or an agent invocation." />
      </div>}
    </Card>
  </div>;
}

const COLUMNS = '84px 44px minmax(200px, 1fr) 44px minmax(200px, 1fr) 110px';

function Totals({ data }) {
  // [label, value of one side, delta field, delta format]. The delta is the
  // server's `b − a`, drawn in the measured hue when it is not zero.
  const rows = [
    ['steps', (s) => fmtNum(s.steps), 'steps', fmtNum],
    ['tokens', (s) => fmtNum(s.tokens), 'tokens', fmtNum],
    ['cost USD', (s) => fmtMoney(s.cost_usd), 'cost_usd', fmtMoney],
    ['duration', (s) => fmtDurationNs(s.duration_ns), 'duration_ns', fmtDurationNs],
    ['errors', (s) => fmtNum(s.errors), 'errors', fmtNum],
    ['outcome', (s) => s.outcome, null, null],
  ];
  return <Card pad="0" style={{ overflow: 'hidden' }}>
    <div className="tz-scroll-x">
      <div style={{ minWidth: 520 }}>
        <div style={{ display: 'grid', gridTemplateColumns: '120px repeat(3, minmax(0, 1fr))', background: 'var(--bg-sunken)', borderBottom: '1px solid var(--hairline)' }}>
          {['', 'a', 'b', 'b − a'].map((label, i) => <div key={i} style={{ padding: '6px 12px', fontSize: 12, fontWeight: 500, color: 'var(--ink-muted)', textAlign: i ? 'right' : 'left' }}>{label}</div>)}
        </div>
        {rows.map(([label, value, field, fmt]) => {
          const delta = field ? data.delta?.[field] : null;
          return <div key={label} data-testid={'total-' + label} style={{ display: 'grid', gridTemplateColumns: '120px repeat(3, minmax(0, 1fr))', borderBottom: '1px solid var(--hairline)', minHeight: 'var(--row-h)', alignItems: 'center' }}>
            <div style={{ padding: 'var(--row-py) 12px', fontSize: 12, color: 'var(--ink-muted)' }}>{label}</div>
            <Cell>{value(data.a)}</Cell>
            <Cell>{value(data.b)}</Cell>
            <Cell tone={delta ? 'var(--accent)' : 'var(--ink-faint)'}>{field ? signed(delta, fmt) : ''}</Cell>
          </div>;
        })}
      </div>
    </div>
    {data.a.truncated || data.b.truncated ? <div style={{ padding: '8px 12px', fontSize: 12, color: 'var(--warn)' }}>
      A side had more than 50,000 spans: its figures describe the first 50,000 and its outcome is unknown.
    </div> : null}
  </Card>;
}

/** A delta with its sign: `+3`, `−0.00120`, `0`. A real minus sign, for
    the same reason format.js uses one in tabular columns. */
function signed(value, fmt) {
  if (typeof value !== 'number' || !Number.isFinite(value)) return '';
  if (value === 0) return '0';
  return (value > 0 ? '+' : '−') + fmt(Math.abs(value));
}

function Cell({ children, tone }) {
  return <div style={{
    padding: 'var(--row-py) 12px', fontFamily: 'var(--font-mono)', fontSize: 12,
    fontVariantNumeric: 'tabular-nums', textAlign: 'right', color: tone || 'var(--ink)', whiteSpace: 'nowrap',
  }}>{children}</div>;
}

function Fingerprint({ value, other }) {
  if (!value) return <span style={{ color: 'var(--ink-faint)' }}>—</span>;
  const fp = splitFingerprint(value);
  const theirs = other ? splitFingerprint(other) : null;
  const nameDiffers = theirs && theirs.name !== fp.name;
  const hashDiffers = theirs && theirs.hash !== fp.hash;
  return <span style={{ fontFamily: 'var(--font-mono)', fontSize: 12, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap', display: 'block' }}>
    <span style={{ color: 'var(--ink-muted)' }}>{fp.kind}:</span>
    <span style={{ color: nameDiffers ? 'var(--accent)' : 'var(--ink)' }}>{fp.name}</span>
    {fp.hash ? <span style={{ color: hashDiffers ? 'var(--accent)' : 'var(--ink-faint)' }}>#{fp.hash}</span> : null}
  </span>;
}

function AlignedRow({ row, first, rowRef, onA, onB }) {
  const indexCell = (index, onClick) => <div role="cell" style={{ padding: 'var(--row-py) 10px' }}>
    {index == null ? <span style={{ color: 'var(--ink-faint)' }}>—</span>
      : <button type="button" className="tz-plain-button" onClick={onClick} title="Open this step in its tape"
        style={{ fontFamily: 'var(--font-mono)', fontSize: 12, color: 'var(--ink)', cursor: 'pointer', textDecoration: 'underline', textDecorationColor: 'var(--ink-faint)' }}>{index}</button>}
  </div>;
  return <div ref={rowRef} role="row" data-testid="aligned-row" data-op={row.op} data-first={first ? 'true' : undefined}
    style={{
      display: 'grid', gridTemplateColumns: COLUMNS, alignItems: 'center', minHeight: 'var(--row-h)',
      borderBottom: '1px solid var(--hairline)', fontSize: 'var(--cell-fs)',
      background: first ? 'var(--accent-tint)' : 'transparent',
      borderLeft: `2px solid ${first ? 'var(--accent)' : 'transparent'}`,
      opacity: row.op === 'same' ? 0.72 : 1,
    }}>
    <div role="cell" style={{ padding: 'var(--row-py) 10px' }}><Tag status={OP_TAG[row.op] || 'neutral'} mono>{row.op}</Tag></div>
    {indexCell(row.a, onA)}
    <div role="cell" style={{ padding: 'var(--row-py) 10px', minWidth: 0 }}><Fingerprint value={row.fingerprint_a} other={row.op === 'changed' ? row.fingerprint_b : null} /></div>
    {indexCell(row.b, onB)}
    <div role="cell" style={{ padding: 'var(--row-py) 10px', minWidth: 0 }}><Fingerprint value={row.fingerprint_b} other={row.op === 'changed' ? row.fingerprint_a : null} /></div>
    <div role="cell" style={{ padding: 'var(--row-py) 10px', fontSize: 12, color: 'var(--ink-muted)' }} title={row.why ? WHY[row.why] : undefined}>
      {row.why || ''}{first ? <span style={{ color: 'var(--accent)' }}> · first</span> : null}
    </div>
  </div>;
}
