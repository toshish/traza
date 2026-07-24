import React from 'react';
import { api, getToken, setToken, onUnauthorized } from './lib/api.js';
import { fmtNum } from './lib/format.js';
import { Logo } from './components/Logo.jsx';
import { Button } from './components/primitives/Button.jsx';
import { Input } from './components/primitives/Input.jsx';
import { Modal } from './components/primitives/Modal.jsx';
import { Tabs } from './components/primitives/Tabs.jsx';
import { Toast } from './components/primitives/Toast.jsx';
import { SpansView } from './views/SpansView.jsx';
import { TraceView } from './views/TraceView.jsx';
import { SessionsView, SessionDetailView } from './views/SessionsView.jsx';
import { AnalyticsView } from './views/AnalyticsView.jsx';
import { StoreView } from './views/StoreView.jsx';

// ------------------------------------------------------------------ routing
// Hash routes, because the server intentionally serves only "/" and
// "/dashboard": #/spans, #/traces/<id>?span=<id>, #/sessions,
// #/sessions/<id>, #/analytics, #/store.

function parseHash(hash) {
  const raw = (hash || '').replace(/^#\/?/, '');
  const [pathPart, queryPart] = raw.split('?');
  const parts = pathPart.split('/').filter(Boolean).map(decodeURIComponent);
  const params = new URLSearchParams(queryPart || '');
  return { parts, params };
}

function useHashRoute() {
  const [route, setRoute] = React.useState(() => parseHash(window.location.hash));
  React.useEffect(() => {
    const onChange = () => setRoute(parseHash(window.location.hash));
    window.addEventListener('hashchange', onChange);
    return () => window.removeEventListener('hashchange', onChange);
  }, []);
  return route;
}

function navigate(parts, params) {
  let hash = '#/' + parts.map(encodeURIComponent).join('/');
  const query = params ? new URLSearchParams(params).toString() : '';
  if (query) hash += '?' + query;
  window.location.hash = hash;
}

// ------------------------------------------------------------------ header

function ThemeToggle() {
  const [dark, setDark] = React.useState(() => document.documentElement.getAttribute('data-theme') === 'dark');
  const toggle = () => {
    const next = !dark;
    setDark(next);
    if (next) document.documentElement.setAttribute('data-theme', 'dark');
    else document.documentElement.removeAttribute('data-theme');
    try { localStorage.setItem('traza_theme', next ? 'dark' : 'light'); } catch (e) {}
  };
  return <button onClick={toggle} title={dark ? 'Switch to light theme' : 'Switch to dark theme'}
    style={{ border: 'none', background: 'transparent', color: 'var(--ink-faint)', cursor: 'pointer', display: 'inline-flex', padding: 4 }}>
    {dark
      ? <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5"><circle cx="12" cy="12" r="4"></circle><path d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4"></path></svg>
      : <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5"><path d="M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8z"></path></svg>}
  </button>;
}

function Header({ recordCount, onSetToken }) {
  return <header style={{ display: 'flex', alignItems: 'center', gap: 12, padding: '10px 16px', borderBottom: '1px solid var(--hairline)', background: 'var(--bg-raised)' }}>
    <a href="#/spans" style={{ display: 'inline-flex', alignItems: 'center', gap: 12, textDecoration: 'none' }}>
      <Logo size={20} />
      <span style={{ fontFamily: 'var(--font-mono)', fontSize: 'var(--text-16)', fontWeight: 500, color: 'var(--ink)', letterSpacing: '-0.01em' }}>traza</span>
    </a>
    <span style={{ fontSize: 'var(--text-12)', color: 'var(--ink-muted)' }}>trace browser</span>
    <span style={{ marginLeft: 'auto', fontFamily: 'var(--font-mono)', fontSize: 'var(--text-12)', fontVariantNumeric: 'tabular-nums', color: 'var(--ink-muted)' }}>
      {recordCount == null ? '' : fmtNum(recordCount) + ' records'}
    </span>
    <ThemeToggle />
    <Button size="sm" onClick={onSetToken}>Set token</Button>
  </header>;
}

// ------------------------------------------------------------------ app

const TABS = [
  { id: 'spans', label: 'Spans' },
  { id: 'sessions', label: 'Sessions' },
  { id: 'analytics', label: 'Analytics' },
  { id: 'store', label: 'Store' },
];

export function App() {
  const route = useHashRoute();
  const [toasts, setToasts] = React.useState([]);
  const [tokenModal, setTokenModal] = React.useState(false);
  const [tokenDraft, setTokenDraft] = React.useState('');
  const [authVersion, setAuthVersion] = React.useState(0);
  const [recordCount, setRecordCount] = React.useState(null);
  const authFailed = React.useRef(false);

  const pushToast = React.useCallback((toast) => {
    const id = Math.random().toString(36).slice(2);
    setToasts((list) => [...list, { ...toast, id }]);
    setTimeout(() => setToasts((list) => list.filter((t) => t.id !== id)), 5000);
  }, []);

  // First 401 anywhere opens the token prompt (once until a token is set).
  React.useEffect(() => {
    onUnauthorized(() => {
      if (!authFailed.current) {
        authFailed.current = true;
        setTokenModal(true);
      }
    });
  }, []);

  // Header record count: poll gently; stop after a 401 until a token is set.
  React.useEffect(() => {
    let live = true;
    const poll = async () => {
      if (authFailed.current) return;
      try {
        const stats = await api.stats();
        if (live) setRecordCount(stats.record_count);
      } catch (e) { /* header stays quiet; views surface errors */ }
    };
    poll();
    const timer = setInterval(poll, 15000);
    return () => { live = false; clearInterval(timer); };
  }, [authVersion]);

  const applyToken = () => {
    setToken(tokenDraft.trim());
    setTokenDraft('');
    setTokenModal(false);
    authFailed.current = false;
    setAuthVersion((v) => v + 1); // remounts the active view so it refetches
  };

  const [head, second] = route.parts;
  const activeTab = head === 'traces' ? 'spans' : (TABS.some((t) => t.id === head) ? head : 'spans');
  const sessionFilter = route.params.get('session') || '';

  let view;
  if (head === 'traces' && second) {
    view = <TraceView key={second + ':' + authVersion} traceId={second}
      selectedSpanId={route.params.get('span') || ''}
      selectSpan={(spanId) => navigate(['traces', second], spanId ? { span: spanId } : undefined)}
      openSession={(id) => navigate(['sessions', id])}
      pushToast={pushToast} />;
  } else if (head === 'sessions' && second) {
    view = <SessionDetailView key={second + ':' + authVersion} sessionId={second}
      openTrace={(traceId) => navigate(['traces', traceId])}
      filterSpans={(id) => navigate(['spans'], { session: id })} />;
  } else if (head === 'sessions') {
    view = <SessionsView key={'sessions:' + authVersion} openSession={(id) => navigate(['sessions', id])} />;
  } else if (head === 'analytics') {
    view = <AnalyticsView key={'analytics:' + authVersion} />;
  } else if (head === 'store') {
    view = <StoreView key={'store:' + authVersion} pushToast={pushToast} />;
  } else {
    view = <SpansView key={'spans:' + sessionFilter + ':' + authVersion}
      sessionFilter={sessionFilter}
      clearSessionFilter={() => navigate(['spans'])}
      openTrace={(traceId, spanId) => navigate(['traces', traceId], spanId ? { span: spanId } : undefined)} />;
  }

  return <div style={{ minHeight: '100vh', background: 'var(--bg)' }}>
    <Header recordCount={recordCount} onSetToken={() => setTokenModal(true)} />
    <main style={{ maxWidth: 1400, margin: '0 auto', padding: '12px 16px' }}>
      <Tabs tabs={TABS} active={activeTab} onChange={(id) => navigate([id])} style={{ marginBottom: 12 }} />
      {view}
    </main>
    <Modal open={tokenModal} title="Set token" onClose={() => setTokenModal(false)} footer={<>
      {getToken() ? <Button onClick={() => { setToken(''); setTokenModal(false); setAuthVersion((v) => v + 1); }}>Clear token</Button> : null}
      <Button variant="primary" onClick={applyToken} disabled={!tokenDraft.trim()}>Set token</Button>
    </>}>
      <div style={{ display: 'grid', gap: 8 }}>
        <div style={{ color: 'var(--ink-muted)' }}>
          The API is gated when <code>TRAZA_TOKENS</code> is set on the server. The token stays in
          this tab's session storage and is sent as a bearer header.
        </div>
        <Input mono type="password" placeholder="token" value={tokenDraft} onChange={setTokenDraft}
          onKeyDown={(e) => { if (e.key === 'Enter' && tokenDraft.trim()) applyToken(); }} />
      </div>
    </Modal>
    <div style={{ position: 'fixed', right: 16, bottom: 16, display: 'grid', gap: 8, zIndex: 60 }}>
      {toasts.map((t) => <Toast key={t.id} status={t.status} title={t.title} detail={t.detail}
        onDismiss={() => setToasts((list) => list.filter((x) => x.id !== t.id))} />)}
    </div>
  </div>;
}
