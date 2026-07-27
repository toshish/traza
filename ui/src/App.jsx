import React from 'react';
import { api, getToken, setToken, onUnauthorized } from './lib/api.js';
import { useHashRoute, navigate, useKeys, usePoll, useStored } from './lib/route.js';
import { NavRail, Header, SCREENS } from './components/nav/Shell.jsx';
import { CommandPalette } from './components/nav/CommandPalette.jsx';
import { Modal } from './components/primitives/Modal.jsx';
import { Button } from './components/primitives/Button.jsx';
import { Input } from './components/primitives/Input.jsx';
import { Toast } from './components/primitives/Toast.jsx';

import { OverviewScreen } from './views/OverviewScreen.jsx';
import { TracesScreen } from './views/TracesScreen.jsx';
import { TraceScreen } from './views/TraceScreen.jsx';
import { SessionsScreen, SessionScreen } from './views/SessionsScreen.jsx';
import { ConversationScreen } from './views/ConversationScreen.jsx';
import { AnalyticsScreen } from './views/AnalyticsScreen.jsx';
import { LatencyScreen } from './views/LatencyScreen.jsx';
import { FailuresScreen } from './views/FailuresScreen.jsx';
import { ScoresScreen } from './views/ScoresScreen.jsx';
import { ExperimentsScreen } from './views/ExperimentsScreen.jsx';
import { DatasetsScreen } from './views/DatasetsScreen.jsx';
import { TailScreen } from './views/TailScreen.jsx';
import { CompareScreen } from './views/CompareScreen.jsx';
import { ServerScreen } from './views/ServerScreen.jsx';
import { StoreScreen } from './views/StoreScreen.jsx';
import { ConnectScreen } from './views/ConnectScreen.jsx';

/** How many samples the rail's ingest sparkline keeps. At a 5s poll this is
    about two minutes of history — long enough to show a shape, short enough
    that a burst three minutes ago is not still claiming the eye. */
const PULSE_SAMPLES = 28;

/** Watches ingest by differencing the admitted-spans counter.

    A rate is not a metric the server keeps — it keeps a counter, which is the
    honest thing to keep — so the client differences it. One small JSON read
    every five seconds serves the pulse, the record count, and the Server
    screen's freshness, instead of three separate polls. */
function useIngestPulse(authVersion) {
  const [pulse, setPulse] = React.useState({ rate: null, spark: [], live: false, durability: null });
  const previous = React.useRef(null);

  const sample = React.useCallback(async () => {
    try {
      const [metrics, stats] = await Promise.all([api.metrics(), api.stats()]);
      const admitted = metrics?.ingest?.spans_admitted ?? 0;
      const now = performance.now();
      const last = previous.current;
      previous.current = { admitted, at: now };
      if (!last) {
        setPulse((p) => ({ ...p, durability: stats.durability, records: stats.record_count }));
        return;
      }
      const seconds = Math.max(0.001, (now - last.at) / 1000);
      const rate = Math.max(0, (admitted - last.admitted) / seconds);
      setPulse((p) => ({
        rate,
        live: rate > 0,
        durability: stats.durability,
        records: stats.record_count,
        spark: [...p.spark, rate].slice(-PULSE_SAMPLES),
      }));
    } catch (e) { /* the rail goes quiet; screens surface their own errors */ }
  }, []);

  React.useEffect(() => { previous.current = null; sample(); }, [sample, authVersion]);
  usePoll(sample, 5000);
  return pulse;
}

export function App() {
  const route = useHashRoute();
  const [toasts, setToasts] = React.useState([]);
  const [tokenModal, setTokenModal] = React.useState(false);
  const [tokenDraft, setTokenDraft] = React.useState('');
  const [authVersion, setAuthVersion] = React.useState(0);
  const [palette, setPalette] = React.useState(false);
  const [density, setDensity] = useStored('traza_density', 'comfortable');
  const [theme, setTheme] = useStored('traza_theme', 'light');
  const authFailed = React.useRef(false);

  const pushToast = React.useCallback((toast) => {
    const id = Math.random().toString(36).slice(2);
    setToasts((list) => [...list, { ...toast, id }]);
    setTimeout(() => setToasts((list) => list.filter((t) => t.id !== id)), 5000);
  }, []);

  // Theme and density are document-level so tokens cascade to every surface,
  // including ones rendered into portals.
  React.useEffect(() => {
    if (theme === 'dark') document.documentElement.setAttribute('data-theme', 'dark');
    else document.documentElement.removeAttribute('data-theme');
  }, [theme]);
  React.useEffect(() => {
    document.documentElement.setAttribute('data-density', density);
  }, [density]);

  // First 401 anywhere opens the token prompt (once until a token is set).
  React.useEffect(() => {
    onUnauthorized(() => {
      if (!authFailed.current) {
        authFailed.current = true;
        setTokenModal(true);
      }
    });
  }, []);

  const pulse = useIngestPulse(authVersion);

  const applyToken = () => {
    setToken(tokenDraft.trim());
    setTokenDraft('');
    setTokenModal(false);
    authFailed.current = false;
    setAuthVersion((v) => v + 1);
  };

  const [head, second, third] = route.parts;
  const screen = head || 'overview';
  const go = React.useCallback((parts, params) => navigate(Array.isArray(parts) ? parts : [parts], params), []);

  useKeys((event, { typing }) => {
    const meta = event.metaKey || event.ctrlKey;
    if (meta && event.key.toLowerCase() === 'k') {
      event.preventDefault();
      setPalette((open) => !open);
      return;
    }
    if (typing || meta || event.altKey) return;
    if (event.key === '?') { event.preventDefault(); go(['connect']); }
  }, [go]);

  const common = { pushToast, go, params: route.params, authVersion };
  let view;
  switch (screen) {
    case 'traces': view = <TracesScreen key={'traces:' + authVersion} {...common} />; break;
    case 'trace': view = <TraceScreen key={'trace:' + second + ':' + authVersion} traceId={second} {...common} />; break;
    case 'sessions':
      view = second
        ? <SessionScreen key={'session:' + second + ':' + authVersion} sessionId={second} {...common} />
        : <SessionsScreen key={'sessions:' + authVersion} {...common} />;
      break;
    case 'conversation':
      view = <ConversationScreen key={'conv:' + second + ':' + third + ':' + authVersion}
        kind={second} id={third} {...common} />;
      break;
    case 'analytics': view = <AnalyticsScreen key={'analytics:' + authVersion} {...common} />; break;
    case 'latency': view = <LatencyScreen key={'latency:' + authVersion} {...common} />; break;
    case 'failures': view = <FailuresScreen key={'failures:' + authVersion} {...common} />; break;
    case 'scores': view = <ScoresScreen key={'scores:' + authVersion} {...common} />; break;
    case 'experiments': view = <ExperimentsScreen key={'experiments:' + authVersion} {...common} />; break;
    case 'datasets': view = <DatasetsScreen key={'datasets:' + authVersion} {...common} />; break;
    case 'tail': view = <TailScreen key={'tail:' + authVersion} {...common} />; break;
    case 'compare': view = <CompareScreen key={'compare:' + authVersion} {...common} />; break;
    case 'server': view = <ServerScreen key={'server:' + authVersion} {...common} />; break;
    case 'store': view = <StoreScreen key={'store:' + authVersion} {...common} />; break;
    case 'connect': view = <ConnectScreen key={'connect:' + authVersion} {...common} />; break;
    default: view = <OverviewScreen key={'overview:' + authVersion} {...common} />;
  }

  // `traces/<id>` and `sessions/<id>` render the detail screens, but the rail
  // should still show which section you are in.
  const railScreen = screen === 'trace' ? 'traces'
    : screen === 'conversation' ? (second === 'sessions' ? 'sessions' : 'traces')
      : screen === 'compare' ? 'traces' : screen;

  return <div style={{
    display: 'flex', minHeight: '100vh', background: 'var(--bg)', color: 'var(--ink)',
    fontFamily: 'var(--font-sans)', fontSize: 13,
  }}>
    <NavRail screen={railScreen} ingest={pulse} onGo={(id) => go([id])} />
    <div style={{ flex: 1, minWidth: 0, display: 'flex', flexDirection: 'column' }}>
      <Header screen={SCREENS[screen] ? screen : 'overview'}
        subtitle={screen === 'trace' && second ? second : undefined}
        recordCount={pulse.records} density={density} onDensity={setDensity}
        theme={theme} onTheme={() => setTheme(theme === 'dark' ? 'light' : 'dark')}
        onPalette={() => setPalette(true)} onToken={() => setTokenModal(true)} />
      <main style={{ flex: 1, minWidth: 0, padding: '16px 20px 64px' }}>{view}</main>
    </div>

    <CommandPalette open={palette} onClose={() => setPalette(false)}
      onNavigate={(parts) => go(parts)} />

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
