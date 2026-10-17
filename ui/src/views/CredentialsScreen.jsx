import React from 'react';
import { api, setToken } from '../lib/api.js';
import { useRead } from '../lib/route.js';
import { fmtAgo, fmtTimeNs } from '../lib/format.js';
import { Card, Chip, EmptyState, ErrorState, Mono, PanelHead, Skeleton } from '../components/primitives/Chrome.jsx';
import { Tag } from '../components/primitives/Tag.jsx';
import { Input } from '../components/primitives/Input.jsx';
import { Modal } from '../components/primitives/Modal.jsx';
import { Button } from '../components/primitives/Button.jsx';
import { CopyButton } from '../components/data/CopyButton.jsx';

// Runtime credentials: mint, list, revoke — the unbound-admin surface under
// /v1/admin/credentials. The store keeps only each token's SHA-256, so a
// minted token is shown exactly once, here, and never again by anything.

const EXPIRIES = [
  { id: 'never', label: 'never', seconds: null },
  { id: '1d', label: '1 day', seconds: 86400 },
  { id: '30d', label: '30 days', seconds: 30 * 86400 },
  { id: '90d', label: '90 days', seconds: 90 * 86400 },
];

/** A credential's state now: revoked, expired, or live. */
export function credentialState(credential, nowNs = Date.now() * 1e6) {
  if (credential.revoked_at_ns) return 'revoked';
  if (credential.expires_at_ns && credential.expires_at_ns <= nowNs) return 'expired';
  return 'live';
}

const STATE_TAG = { live: 'ok', revoked: 'error', expired: 'warn' };

export function CredentialsScreen({ identity, pushToast, onUseToken }) {
  const list = useRead((signal) => api.credentials(signal), []);
  const [minted, setMinted] = React.useState(null);
  const [revoking, setRevoking] = React.useState(null);
  const credentials = list.data?.credentials || [];
  const open = identity?.auth === 'open';

  if (!identity) return <Skeleton height={160} />;
  return <div style={{ display: 'grid', gap: 14, maxWidth: 1500 }}>
    {open ? <Card style={{ borderLeft: '2px solid var(--warn)' }}>
      <div style={{ fontSize: 13, lineHeight: '20px', color: 'var(--ink)' }}>
        This store is open: no credential exists, so every client is an unbound admin. Minting one
        switches authentication on for every client from that request on — this tab included — and
        the credential log keeps it on across restarts. The first credential must be an unbound{' '}
        <code>admin</code>; anything else is refused.
      </div>
    </Card> : null}

    <MintCard open={open} credentials={credentials} onMinted={(doc) => {
      if (open) {
        // The store closed with that request: every request from here on,
        // this tab's polls included, needs a token, and the only one that
        // exists is the one just minted. Using it at once keeps the next
        // poll from answering 401 and covering the one-time display with a
        // token prompt. The identity is refreshed when the dialog closes.
        setToken(doc.token);
        setMinted({ ...doc, adopted: true });
      } else {
        setMinted(doc);
      }
      list.reload();
    }} />

    <Card pad="0" style={{ overflow: 'hidden' }}>
      <div style={{ padding: '12px 14px 0' }}>
        <PanelHead title="Credentials" note="environment ones first, then runtime ones in creation order; never a token or a digest" />
      </div>
      {list.error ? <div style={{ padding: '0 14px 12px' }}>
        <ErrorState what={list.error.what} next={list.error.next} onRetry={list.reload} />
      </div> : null}
      {list.loading && !list.data ? <div style={{ padding: '0 14px 14px' }}><Skeleton height={60} /></div> : null}
      {list.data && !credentials.length ? <div style={{ padding: '0 14px 14px' }}>
        <EmptyState message="No credentials exist: the store is open." />
      </div> : null}
      {credentials.length ? <CredentialTable credentials={credentials} self={identity.credential?.id}
        onRevoke={(credential) => setRevoking(credential)} /> : null}
      <div style={{ padding: '10px 14px', fontSize: 12, color: 'var(--ink-muted)' }}>
        <code>last used</code> is kept in server memory only; a restart forgets it.
      </div>
    </Card>

    <TokenOnce doc={minted}
      onClose={() => { const adopted = minted?.adopted ? minted.token : null; setMinted(null); if (adopted) onUseToken?.(adopted); }}
      onUse={(token) => { setMinted(null); onUseToken?.(token); }} />
    <RevokeModal credential={revoking} self={identity.credential?.id} onClose={() => setRevoking(null)}
      onRevoked={(credential) => {
        setRevoking(null);
        list.reload();
        pushToast({ status: 'ok', title: 'Credential revoked', detail: `${credential.id}: its token answers 401 from the next request` });
      }} />
  </div>;
}

function MintCard({ open, credentials, onMinted }) {
  const first = open && !credentials.length;
  const [scope, setScope] = React.useState(first ? 'admin' : 'ro');
  const [tenant, setTenant] = React.useState('');
  const [label, setLabel] = React.useState('');
  const [expiry, setExpiry] = React.useState('never');
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState(null);

  const mint = async () => {
    setBusy(true);
    setError(null);
    try {
      const body = { scope };
      if (tenant.trim()) body.tenant = tenant.trim();
      if (label.trim()) body.label = label.trim();
      const seconds = EXPIRIES.find((e) => e.id === expiry).seconds;
      if (seconds) body.expires_in_seconds = seconds;
      const doc = await api.mintCredential(body);
      setTenant('');
      setLabel('');
      onMinted(doc);
    } catch (e) { setError(e); } finally { setBusy(false); }
  };

  return <Card>
    <PanelHead title="Mint a credential" note="a tenant binding holds the token to that tenant's data on every surface" />
    <div className="tz-reflow-1" style={{ display: 'grid', gridTemplateColumns: 'auto 1fr 1fr', gap: 10, alignItems: 'end' }}>
      <div style={{ display: 'grid', gap: 4 }}>
        <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>scope</span>
        <div style={{ display: 'flex', gap: 6 }}>
          {['ro', 'rw', 'admin'].map((s) => <Chip key={s} mono active={scope === s} onClick={() => setScope(s)}>{s}</Chip>)}
        </div>
      </div>
      <label style={{ display: 'grid', gap: 4, fontSize: 12, color: 'var(--ink-muted)' }}>tenant (empty: unbound)
        <Input size="sm" mono value={tenant} onChange={setTenant} placeholder="acme" />
      </label>
      <label style={{ display: 'grid', gap: 4, fontSize: 12, color: 'var(--ink-muted)' }}>label
        <Input size="sm" value={label} onChange={setLabel} placeholder="acme collector" />
      </label>
    </div>
    <div style={{ display: 'flex', gap: 6, alignItems: 'center', flexWrap: 'wrap', marginTop: 10 }}>
      <span style={{ fontSize: 12, color: 'var(--ink-muted)' }}>expires</span>
      {EXPIRIES.map((e) => <Chip key={e.id} active={expiry === e.id} onClick={() => setExpiry(e.id)}>{e.label}</Chip>)}
      <span style={{ marginLeft: 'auto' }}>
        <Chip tone="primary" onClick={busy ? undefined : mint}>{busy ? 'Minting…' : 'Mint'}</Chip>
      </span>
    </div>
    <div style={{ fontSize: 12, color: 'var(--ink-muted)', marginTop: 8, lineHeight: '19px' }}>
      Give collectors <code>rw</code>, dashboards and alerting <code>ro</code>, and keep{' '}
      <code>admin</code> for operators: it can erase, and an unbound admin can mint more credentials.
    </div>
    {error ? <ErrorState what={error.what} next={error.next} style={{ marginTop: 10 }} /> : null}
  </Card>;
}

const COLUMNS = '150px 120px 64px 110px minmax(120px, 1fr) 54px 96px 96px 80px 80px';

function CredentialTable({ credentials, self, onRevoke }) {
  const now = Date.now() * 1e6;
  return <div className="tz-scroll-x">
    <div style={{ minWidth: 1040 }} role="table" aria-label="Credentials">
      <div role="row" style={{ display: 'grid', gridTemplateColumns: COLUMNS, background: 'var(--bg-sunken)', borderTop: '1px solid var(--hairline)', borderBottom: '1px solid var(--hairline)' }}>
        {['id', 'prefix', 'scope', 'tenant', 'label', 'source', 'created', 'expires', 'last used', ''].map((label, i) => <div key={i} role="columnheader" style={{ padding: '6px 10px', fontSize: 12, fontWeight: 500, color: 'var(--ink-muted)', whiteSpace: 'nowrap' }}>{label}</div>)}
      </div>
      {credentials.map((credential) => {
        const state = credentialState(credential, now);
        return <div key={credential.id} role="row" data-testid="credential-row" style={{
          display: 'grid', gridTemplateColumns: COLUMNS, alignItems: 'center', minHeight: 'var(--row-h)',
          borderBottom: '1px solid var(--hairline)', fontSize: 'var(--cell-fs)', opacity: state === 'live' ? 1 : 0.65,
        }}>
          <div role="cell" style={{ padding: 'var(--row-py) 10px' }}>
            <Mono color="var(--ink)">{credential.id}</Mono>
            {credential.id === self ? <div style={{ fontSize: 11, color: 'var(--ink-faint)' }}>this tab</div> : null}
          </div>
          <div role="cell" style={{ padding: 'var(--row-py) 10px' }}><Mono color="var(--ink-muted)">{credential.prefix}</Mono></div>
          <div role="cell" style={{ padding: 'var(--row-py) 10px' }}><Mono color="var(--ink)">{credential.scope}</Mono></div>
          <div role="cell" style={{ padding: 'var(--row-py) 10px' }}><Mono color={credential.tenant ? 'var(--ink)' : 'var(--ink-faint)'}>{credential.tenant || 'unbound'}</Mono></div>
          <div role="cell" style={{ padding: 'var(--row-py) 10px', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{credential.label || <span style={{ color: 'var(--ink-faint)' }}>—</span>}</div>
          <div role="cell" style={{ padding: 'var(--row-py) 10px' }}><Mono color="var(--ink-muted)">{credential.source}</Mono></div>
          <div role="cell" style={{ padding: 'var(--row-py) 10px' }} title={fmtTimeNs(credential.created_at_ns)}><Mono color="var(--ink-muted)">{fmtAgo(credential.created_at_ns)} ago</Mono></div>
          <div role="cell" style={{ padding: 'var(--row-py) 10px' }} title={credential.expires_at_ns ? fmtTimeNs(credential.expires_at_ns) : 'no expiry'}>
            <Mono color="var(--ink-muted)">{credential.expires_at_ns ? fmtTimeNs(credential.expires_at_ns).slice(0, 10) : 'never'}</Mono>
          </div>
          <div role="cell" style={{ padding: 'var(--row-py) 10px' }}><Mono color="var(--ink-muted)">{credential.last_used_at_ns ? fmtAgo(credential.last_used_at_ns) + ' ago' : '—'}</Mono></div>
          <div role="cell" style={{ padding: 'var(--row-py) 10px', display: 'flex', justifyContent: 'flex-end', gap: 6 }}>
            {state !== 'live' ? <Tag status={STATE_TAG[state]} mono>{state}</Tag>
              : credential.source === 'api' ? <Chip onClick={() => onRevoke(credential)}>Revoke</Chip>
                : <span title="Environment credentials are revoked by restarting without them" style={{ fontSize: 11, color: 'var(--ink-faint)' }}>env</span>}
          </div>
        </div>;
      })}
    </div>
  </div>;
}

function TokenOnce({ doc, onClose, onUse }) {
  if (!doc) return null;
  return <Modal open title="Credential minted" width={600} onClose={onClose} footer={<>
    {doc.adopted ? null
      : <Button onClick={() => onUse(doc.token)} title="Use this token in this browser tab">Use in this tab</Button>}
    <Button variant="primary" onClick={onClose}>Done</Button>
  </>}>
    <div style={{ display: 'grid', gap: 10 }}>
      <div style={{ color: 'var(--ink)', lineHeight: '19px' }}>
        Copy the token now. It is in this response and nowhere else, ever: the store keeps only its
        SHA-256. Lose it and revoke the credential.
        {doc.adopted ? <> The store was open until now, so this tab has switched to this token to keep
          working; it stays in this tab's session storage only.</> : null}
      </div>
      <div data-testid="minted-token" style={{
        display: 'flex', alignItems: 'center', gap: 8, padding: '8px 10px', background: 'var(--bg-sunken)',
        border: '1px solid var(--hairline)', borderRadius: 'var(--radius-control)', minWidth: 0,
      }}>
        <Mono size={12} color="var(--ink)" style={{ overflowWrap: 'anywhere', flex: 1 }}>{doc.token}</Mono>
        <CopyButton text={doc.token} label="copy" />
      </div>
      <div style={{ display: 'flex', gap: 14, flexWrap: 'wrap', fontSize: 12, color: 'var(--ink-muted)' }}>
        <span>id <Mono color="var(--ink)">{doc.id}</Mono></span>
        <span>scope <Mono color="var(--ink)">{doc.scope}</Mono></span>
        <span>tenant <Mono color="var(--ink)">{doc.tenant || 'unbound'}</Mono></span>
        <span>expires <Mono>{doc.expires_at_ns ? fmtTimeNs(doc.expires_at_ns) : 'never'}</Mono></span>
      </div>
    </div>
  </Modal>;
}

function RevokeModal({ credential, self, onClose, onRevoked }) {
  const [busy, setBusy] = React.useState(false);
  const [error, setError] = React.useState(null);
  React.useEffect(() => { setError(null); }, [credential]);
  if (!credential) return null;
  const revoke = async () => {
    setBusy(true);
    setError(null);
    try {
      await api.revokeCredential(credential.id);
      onRevoked(credential);
    } catch (e) { setError(e); } finally { setBusy(false); }
  };
  return <Modal open title="Revoke credential" onClose={onClose} footer={<>
    <Button onClick={onClose}>Cancel</Button>
    <Button variant="danger" onClick={revoke} disabled={busy}>Revoke {credential.id}</Button>
  </>}>
    <div style={{ display: 'grid', gap: 8, color: 'var(--ink-muted)', lineHeight: '19px' }}>
      <div>
        From the next request its token (<Mono color="var(--ink)">{credential.prefix}…</Mono>
        {credential.label ? <>, {credential.label}</> : null}) answers <code>401</code>, like an unknown
        one. The revocation is durable and cannot be undone: a revoked token can never be registered
        again.
      </div>
      {credential.id === self ? <div style={{ color: 'var(--error)' }}>
        This is the credential this tab is using. Revoking it signs this tab out.
      </div> : null}
      {error ? <ErrorState what={error.what} next={error.next} /> : null}
    </div>
  </Modal>;
}
