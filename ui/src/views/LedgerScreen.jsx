import React from 'react';
import { api } from '../lib/api.js';
import { useRead } from '../lib/route.js';
import { fmtNum, fmtTimeNs } from '../lib/format.js';
import { Card, Chip, EmptyState, ErrorState, LoadingBar, Mono, PanelHead, Skeleton } from '../components/primitives/Chrome.jsx';
import { Tag } from '../components/primitives/Tag.jsx';
import { Input } from '../components/primitives/Input.jsx';
import { CopyButton } from '../components/data/CopyButton.jsx';

// The ledger: a hash chain over everything the store committed — every
// published generation, every erasure, every external anchor — and the
// evidence pack that bundles it for an auditor.
//
// This screen shows what the server says and does not recompute a head
// itself. Ledger timestamps exceed 2^53, and a browser's JSON.parse rounds
// them, so a head recomputed here would fail to match for reasons that have
// nothing to do with tampering. The server verifies; the guide has a
// ten-line script for checking it independently.

const PAGE = 200;

const WINDOWS = [
  { id: '24h', label: '24h', ms: 24 * 3600e3 },
  { id: '7d', label: '7d', ms: 7 * 24 * 3600e3 },
  { id: '30d', label: '30d', ms: 30 * 24 * 3600e3 },
  { id: 'all', label: 'all time', ms: null },
];

export function LedgerScreen({ identity, pushToast }) {
  const operator = Boolean(identity && identity.operator);
  const head = useRead((signal) => api.ledgerHead(signal), []);

  if (!identity) return <Skeleton height={160} />;
  return <div style={{ display: 'grid', gap: 14, maxWidth: 1500 }}>
    <HeadCard head={head} />
    <EvidenceCard identity={identity} operator={operator} pushToast={pushToast} />
    {operator ? <>
      <VerifyCard />
      <EntriesCard />
    </> : <Card>
      <div style={{ fontSize: 13, color: 'var(--ink-muted)', lineHeight: '20px' }}>
        The chain's entries and its verification are operator surfaces: they carry every tenant's
        erasure entries. Your evidence pack above carries this tenant's view of both — its own
        erasure entries whole, other tenants' as stubs that keep the chain followable, and the
        store's verification verdict.
      </div>
    </Card>}
  </div>;
}

function HeadCard({ head }) {
  const data = head.data;
  return <Card>
    <PanelHead title="Head" note="the chain's tip; anchor it with a notary to make later rewrites detectable" />
    {head.error ? <ErrorState what={head.error.what} next={head.error.next} onRetry={head.reload} /> : null}
    {!data && !head.error ? <Skeleton height={40} /> : null}
    {data ? <div data-testid="ledger-head" style={{ display: 'grid', gap: 6 }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 8, minWidth: 0 }}>
        <Mono size={13} color="var(--accent)" style={{ overflowWrap: 'anywhere' }}>{data.head}</Mono>
        <CopyButton text={data.head} />
      </div>
      <div style={{ display: 'flex', gap: 16, flexWrap: 'wrap', fontSize: 12, color: 'var(--ink-muted)' }}>
        <span>seq <Mono color="var(--ink)">{fmtNum(data.seq)}</Mono></span>
        <span>store <Mono color="var(--ink)">{data.store_id}</Mono></span>
        <span>appended <Mono>{fmtTimeNs(data.at_ns)}</Mono></span>
      </div>
    </div> : null}
  </Card>;
}

function EvidenceCard({ identity, operator, pushToast }) {
  const [windowId, setWindowId] = React.useState('all');
  const [scope, setScope] = React.useState('store');
  const [tenant, setTenant] = React.useState('');
  const [busy, setBusy] = React.useState(false);
  const [last, setLast] = React.useState(null);
  const [error, setError] = React.useState(null);

  const download = async () => {
    setBusy(true);
    setError(null);
    try {
      const params = {};
      const window = WINDOWS.find((w) => w.id === windowId);
      if (window.ms) {
        const nowMs = Date.now();
        params.since = String(Math.round((nowMs - window.ms) * 1e6));
        params.until = String(Math.round(nowMs * 1e6));
      }
      // An unbound caller names a tenant (an empty name is the default
      // tenant) or names none for the whole store. A bound caller always
      // gets its own pack and sends nothing.
      let label = identity.tenant || 'store';
      if (operator && scope === 'tenant') {
        params.tenant = tenant.trim();
        label = tenant.trim() || 'default';
      }
      const text = await api.evidenceText(params);
      const summary = summarize(text);
      saveText(text, `traza-evidence-${label}-${new Date().toISOString().slice(0, 10)}.json`);
      setLast(summary);
      pushToast({ status: 'ok', title: 'Evidence pack downloaded', detail: summary.digest ? 'digest ' + summary.digest.slice(0, 16) + '…' : undefined });
    } catch (e) {
      setError(e);
    } finally { setBusy(false); }
  };

  return <Card>
    <PanelHead title="Evidence pack"
      note="the chain, its verification, retention, activity and erasures, under one digest for a notary to sign" />
    <div style={{ display: 'flex', gap: 6, flexWrap: 'wrap', alignItems: 'center', marginBottom: 10 }}>
      <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>window</span>
      {WINDOWS.map((w) => <Chip key={w.id} mono active={windowId === w.id} onClick={() => setWindowId(w.id)}>{w.label}</Chip>)}
      {operator ? <>
        <span style={{ fontSize: 12, color: 'var(--ink-muted)', marginLeft: 10 }}>cover</span>
        <Chip active={scope === 'store'} onClick={() => setScope('store')}>whole store</Chip>
        <Chip active={scope === 'tenant'} onClick={() => setScope('tenant')}>one tenant</Chip>
        {scope === 'tenant' ? <Input size="sm" mono value={tenant} onChange={setTenant}
          placeholder="tenant (empty: default)" ariaLabel="Tenant to cover" style={{ width: 190 }} /> : null}
      </> : <span style={{ fontSize: 12, color: 'var(--ink-muted)', marginLeft: 10 }}>
        covers tenant <Mono color="var(--ink)">{identity.tenant || '(default)'}</Mono>
      </span>}
      <span style={{ marginLeft: 'auto' }}>
        <Chip tone="primary" onClick={busy ? undefined : download}>{busy ? 'Building…' : 'Download evidence pack'}</Chip>
      </span>
    </div>
    <div style={{ fontSize: 12, color: 'var(--ink-muted)', lineHeight: '19px' }}>
      The file is saved exactly as the server sent it. Its <code>digest</code> is over the canonical
      JSON of every other field, and its timestamps exceed 2^53 — re-serializing it with a
      doubles-only JSON parser would change the bytes the digest covers. A window too large to fold
      within the server's query deadline is refused rather than cut short; narrow it.
    </div>
    {error ? <ErrorState what={error.what} next={error.next} style={{ marginTop: 10 }} /> : null}
    {last ? <div data-testid="evidence-summary" style={{ marginTop: 10, display: 'flex', gap: 14, flexWrap: 'wrap', fontSize: 12, color: 'var(--ink-muted)' }}>
      <span>digest <Mono color="var(--accent)">{last.digest}</Mono></span>
      {last.spans != null ? <span>spans <Mono color="var(--ink)">{fmtNum(last.spans)}</Mono></span> : null}
      {last.entries != null ? <span>ledger entries <Mono color="var(--ink)">{fmtNum(last.entries)}</Mono></span> : null}
      {last.intact != null ? <Tag status={last.intact ? 'ok' : 'error'} mono>{last.intact ? 'intact' : 'broken'}</Tag> : null}
    </div> : null}
  </Card>;
}

/** A pack's headline figures, for the confirmation line. Parsing for display
    is fine; the saved file is the original text. */
export function summarize(text) {
  try {
    const pack = JSON.parse(text);
    return {
      digest: pack.digest,
      spans: pack.activity?.spans,
      entries: pack.ledger?.entries?.length,
      intact: pack.ledger?.verification?.intact,
    };
  } catch (e) {
    return { digest: (/"digest":"([0-9a-f]{64})"/.exec(text) || [])[1] };
  }
}

function saveText(text, filename) {
  const url = URL.createObjectURL(new Blob([text], { type: 'application/json' }));
  const anchor = document.createElement('a');
  anchor.href = url;
  anchor.download = filename;
  anchor.click();
  setTimeout(() => URL.revokeObjectURL(url), 0);
}

function VerifyCard() {
  const [result, setResult] = React.useState(null);
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState(null);
  const verify = async () => {
    setBusy(true);
    setError(null);
    try { setResult(await api.ledgerVerify()); } catch (e) { setError(e); } finally { setBusy(false); }
  };
  return <Card>
    <PanelHead title="Verify"
      note="recomputes every head and link, checks the live generation against the latest checkpoint, re-digests its files"
      action={<Chip tone="primary" onClick={busy ? undefined : verify}>{busy ? 'Verifying…' : 'Verify'}</Chip>} />
    <div style={{ fontSize: 12, color: 'var(--ink-muted)', lineHeight: '19px' }}>
      The file check reads every byte the live manifest names, so it costs as much as{' '}
      <code>GET /v1/verify</code>. Compaction between checkpoints can leave the manifest naming files
      that are gone; checkpoint first for a clean check.
    </div>
    <LoadingBar active={busy} style={{ marginTop: 8 }} />
    {error ? <ErrorState what={error.what} next={error.next} style={{ marginTop: 10 }} /> : null}
    {result ? <div data-testid="ledger-verify" style={{ marginTop: 10, display: 'grid', gap: 8 }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 10, flexWrap: 'wrap', fontSize: 12, color: 'var(--ink-muted)' }}>
        <Tag status={result.intact ? 'ok' : 'error'} mono>{result.intact ? 'intact' : 'broken'}</Tag>
        <span><Mono color="var(--ink)">{fmtNum(result.entries)}</Mono> entries</span>
        {result.generation ? <>
          <span>generation <Mono color="var(--ink)">{result.generation.id}</Mono></span>
          <span>manifest {result.generation.matches_ledger ? 'matches the ledger' : <span style={{ color: 'var(--error)' }}>does not match the ledger</span>}</span>
          <span>files {result.generation.files_verified ? 'verified' : <span style={{ color: 'var(--error)' }}>not verified</span>}</span>
        </> : null}
      </div>
      {result.problems?.length ? <ul style={{ margin: 0, paddingLeft: 18, display: 'grid', gap: 3 }}>
        {result.problems.map((problem, i) => <li key={i} style={{ fontFamily: 'var(--font-mono)', fontSize: 12, color: 'var(--error)', overflowWrap: 'anywhere' }}>
          <span style={{ color: 'var(--ink)' }}>{problem}</span>
        </li>)}
      </ul> : <div style={{ fontSize: 12, color: 'var(--ink-muted)' }}>No problems.</div>}
    </div> : null}
  </Card>;
}

function EntriesCard() {
  const [pages, setPages] = React.useState([]);
  const [more, setMore] = React.useState({ busy: false, error: null });
  const first = useRead((signal) => api.ledger({ limit: PAGE }, signal), []);
  const entries = [...(first.data?.entries || []), ...pages.flat()];
  const headSeq = first.data?.head?.seq;
  const lastSeq = entries.length ? entries[entries.length - 1].seq : null;
  const remaining = headSeq != null && lastSeq != null && lastSeq < headSeq;
  const anchors = entries.filter((entry) => entry.kind === 'anchor');

  const loadMore = async () => {
    setMore({ busy: true, error: null });
    try {
      const page = await api.ledger({ after: lastSeq, limit: PAGE });
      setPages((list) => [...list, page.entries || []]);
      setMore({ busy: false, error: null });
    } catch (e) { setMore({ busy: false, error: e }); }
  };

  return <>
    <Card pad="0" style={{ overflow: 'hidden' }}>
      <div style={{ padding: '12px 14px 0' }}>
        <PanelHead title="Anchors" note={remaining ? 'among the entries loaded below' : 'external countersignatures over earlier heads, stored opaque'} />
      </div>
      {anchors.length ? <EntryTable entries={anchors} /> : <div style={{ padding: '0 14px 14px', fontSize: 13, color: 'var(--ink-muted)' }}>
        No anchors{remaining ? ' among the loaded entries' : ''}. Without one the chain proves the current state
        matches a committed manifest, but not that history was never rewritten:{' '}
        <code>POST /v1/ledger/anchors</code> records a notary's signature over a head.
      </div>}
    </Card>
    <Card pad="0" style={{ overflow: 'hidden' }}>
      <div style={{ padding: '12px 14px 0' }}>
        <PanelHead title="Entries" note={`${fmtNum(entries.length)} of ${headSeq == null ? '…' : fmtNum(headSeq + 1)}, in chain order`} />
      </div>
      {first.error ? <div style={{ padding: '0 14px 12px' }}><ErrorState what={first.error.what} next={first.error.next} onRetry={first.reload} /></div> : null}
      {first.loading && !first.data ? <div style={{ padding: '0 14px 14px' }}><Skeleton height={60} /></div> : null}
      {entries.length ? <EntryTable entries={entries} /> : first.data ? <div style={{ padding: '0 14px 14px' }}>
        <EmptyState message="The ledger has no entries yet." />
      </div> : null}
      {remaining ? <div style={{ padding: '10px 12px' }}>
        <Chip onClick={more.busy ? undefined : loadMore}>{more.busy ? 'Loading…' : 'Load more'}</Chip>
        {more.error ? <span style={{ marginLeft: 10, fontSize: 12, color: 'var(--error)' }}>{more.error.what}</span> : null}
      </div> : null}
    </Card>
  </>;
}

const ENTRY_COLUMNS = '60px 96px 168px 150px minmax(260px, 1fr)';

/** What an entry says beyond the common fields, per kind. */
function entryDetail(entry) {
  switch (entry.kind) {
    case 'genesis': return <>store <Mono color="var(--ink)">{entry.store_id}</Mono></>;
    case 'checkpoint': return <>generation <Mono color="var(--ink)">{entry.generation}</Mono> · manifest <Mono>{String(entry.manifest_sha256 || '').slice(0, 16)}…</Mono></>;
    case 'erasure': return entry.redacted
      ? <span>another tenant's erasure (redacted)</span>
      : <>erasure <Mono color="var(--ink)">{entry.erasure_id}</Mono> · {entry.subject_kind}
        {entry.tenant != null ? <> · tenant <Mono>{entry.tenant || '(default)'}</Mono></> : null}
        {' '}· subject <Mono>{String(entry.subject_digest || '').slice(0, 12)}…</Mono></>;
    case 'anchor': return <>{entry.authority} over seq <Mono color="var(--ink)">{entry.target_seq}</Mono>
      {entry.note ? <> · {entry.note}</> : null}</>;
    default: return null;
  }
}

function EntryTable({ entries }) {
  return <div className="tz-scroll-x">
    <div style={{ minWidth: 760 }} role="table" aria-label="Ledger entries">
      <div role="row" style={{ display: 'grid', gridTemplateColumns: ENTRY_COLUMNS, background: 'var(--bg-sunken)', borderTop: '1px solid var(--hairline)', borderBottom: '1px solid var(--hairline)' }}>
        {['seq', 'kind', 'appended', 'head', 'detail'].map((label) => <div key={label} role="columnheader" style={{ padding: '6px 10px', fontSize: 12, fontWeight: 500, color: 'var(--ink-muted)' }}>{label}</div>)}
      </div>
      {entries.map((entry) => <div key={entry.seq + ':' + entry.kind} role="row" data-testid="ledger-entry" style={{
        display: 'grid', gridTemplateColumns: ENTRY_COLUMNS, alignItems: 'center', minHeight: 'var(--row-h)',
        borderBottom: '1px solid var(--hairline)', fontSize: 'var(--cell-fs)',
      }}>
        <div role="cell" style={{ padding: 'var(--row-py) 10px' }}><Mono color="var(--ink)">{entry.seq}</Mono></div>
        <div role="cell" style={{ padding: 'var(--row-py) 10px' }}><Tag mono status={entry.kind === 'anchor' ? 'accent' : 'neutral'}>{entry.kind}</Tag></div>
        <div role="cell" style={{ padding: 'var(--row-py) 10px' }}><Mono color="var(--ink-muted)">{fmtTimeNs(entry.at_ns)}</Mono></div>
        <div role="cell" style={{ padding: 'var(--row-py) 10px' }} title={entry.head}><Mono color="var(--ink-muted)">{String(entry.head || '').slice(0, 16)}…</Mono></div>
        <div role="cell" style={{ padding: 'var(--row-py) 10px', fontSize: 12, color: 'var(--ink-muted)', minWidth: 0 }}>{entryDetail(entry)}</div>
      </div>)}
    </div>
  </div>;
}
