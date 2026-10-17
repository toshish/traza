import React from 'react';
import { api, getToken, setToken, onUnauthorized } from './lib/api.js';
import { setIdentity } from './lib/session.js';
import { useHashRoute, navigate, useKeys, usePoll, useStored, useSystemTheme } from './lib/route.js';
import { NavRail, Header, SCREENS, OPERATOR_SCREENS, navFor } from './components/nav/Shell.jsx';
import { CommandPalette } from './components/nav/CommandPalette.jsx';
import { Modal } from './components/primitives/Modal.jsx';
import { Button } from './components/primitives/Button.jsx';
import { Input } from './components/primitives/Input.jsx';
import { Toast } from './components/primitives/Toast.jsx';
import { EmptyState, Skeleton } from './components/primitives/Chrome.jsx';

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
import { McpScreen } from './views/McpScreen.jsx';
import { GuardScreen } from './views/GuardScreen.jsx';
import { RunScreen } from './views/RunScreen.jsx';
import { DiffScreen } from './views/DiffScreen.jsx';
import { LedgerScreen } from './views/LedgerScreen.jsx';
import { CredentialsScreen } from './views/CredentialsScreen.jsx';

/** How many samples the rail's ingest sparkline keeps. At a 5s poll this is
    about two minutes of history — long enough to show a shape, short enough
    that a burst three minutes ago is not still claiming the eye. */
const PULSE_SAMPLES = 28;

/** Asks the server what the current credential is, once per token.

    Everything tenant-aware hangs off this answer: which screens are offered,
    which surfaces are polled, what the rail says you are looking at. Until it
    arrives the identity is `null`, and nothing that depends on it runs — a
    bound token must never have asked an operator surface "just while we
    found out". The answer is also handed to the API client, which refuses
    operator paths for a non-operator itself. */
function useIdentity(authVersion) {
  const [identity, setState] = React.useState(null);
  React.useEffect(() => {
    let live = true;
    setState(null);
    setIdentity(null);
    api.whoami().then(
      (who) => {
        if (!live) return;
        setIdentity(who);
        setState(who);
      },
      (error) => {
        if (!live) return;
        if (error && error.status === 401) {
          setState({ unauthorized: true });
          return;
        }
        // Unknown is treated as "not an operator": the safe reading of a
        // credential nobody has described is the one that never touches a
        // store-global surface.
        const unknown = { auth: 'unknown', operator: false, scope: null, tenant: null, error };
        setIdentity(unknown);
        setState(unknown);
      },
    );
    return () => { live = false; };
  }, [authVersion]);
  return identity;
}

/** Watches ingest by differencing a counter.

    A rate is not a metric the server keeps — it keeps counters, which is the
    honest thing to keep — so the client differences one. Which counter
    depends on who is asking. An operator reads `/v1/metrics.json` (spans
    admitted by the whole process) and `/v1/stats` (records held and the
    durability mode). A tenant-bound credential may not read either — they
    disclose co-tenants' volume — so it differences its own row of
    `/v1/usage` (spans its tenant has sent), which is also what its header
    count shows. One small read every five seconds either way. */
function useIngestPulse(identity, authVersion) {
  const [pulse, setPulse] = React.useState({ rate: null, spark: [], live: false, durability: null, count: null });
  const previous = React.useRef(null);
  const mode = !identity || identity.unauthorized ? null : identity.operator ? 'operator' : 'tenant';

  const sample = React.useCallback(async () => {
    if (!mode) return;
    try {
      let admitted;
      let count;
      let durability = null;
      if (mode === 'operator') {
        const [metrics, stats] = await Promise.all([api.metrics(), api.stats()]);
        admitted = metrics?.ingest?.spans_admitted ?? 0;
        durability = stats.durability;
        count = { value: stats.record_count, label: 'records', title: 'Records held by the store (GET /v1/stats)' };
      } else {
        const usage = await api.usage();
        const rows = usage?.tenants || [];
        const row = rows.find((r) => r.tenant === (identity.tenant || '')) || rows[0];
        admitted = row?.lifetime?.spans ?? 0;
        count = {
          value: admitted, label: 'spans sent',
          title: 'Spans this tenant has sent since its counters began (GET /v1/usage)',
        };
      }
      const now = performance.now();
      const last = previous.current;
      previous.current = { admitted, at: now };
      if (!last) {
        setPulse((p) => ({ ...p, durability, count }));
        return;
      }
      const seconds = Math.max(0.001, (now - last.at) / 1000);
      const rate = Math.max(0, (admitted - last.admitted) / seconds);
      setPulse((p) => ({
        rate,
        live: rate > 0,
        durability,
        count,
        spark: [...p.spark, rate].slice(-PULSE_SAMPLES),
      }));
    } catch (e) { /* the rail goes quiet; screens surface their own errors */ }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [mode, identity?.tenant]);

  React.useEffect(() => {
    previous.current = null;
    setPulse({ rate: null, spark: [], live: false, durability: null, count: null });
    sample();
  }, [sample, authVersion]);
  usePoll(sample, 5000, Boolean(mode));
  return pulse;
}

export function App() {
  const route = useHashRoute();
  const [toasts, setToasts] = React.useState([]);
  const [tokenModal, setTokenModal] = React.useState(false);
  const [tokenDraft, setTokenDraft] = React.useState('');
  const [authVersion, setAuthVersion] = React.useState(0);
  const [palette, setPalette] = React.useState(false);
  const [drawer, setDrawer] = React.useState(false);
  const [density, setDensity] = useStored('traza_density', 'comfortable');
  // `null` means "no choice made": follow the OS, live. Only a click on the
  // toggle stores a choice. Defaulting to 'light' (and storing it on the first
  // render) meant prefers-color-scheme was read once by index.html and then
  // overruled forever.
  const [themeChoice, setThemeChoice] = useStored('traza_theme', null);
  const systemTheme = useSystemTheme();
  const theme = themeChoice === 'dark' || themeChoice === 'light' ? themeChoice : systemTheme;
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
  // The API client has already stopped sending by then; see session.js.
  React.useEffect(() => {
    onUnauthorized(() => {
      if (!authFailed.current) {
        authFailed.current = true;
        setTokenModal(true);
      }
    });
  }, []);

  const identity = useIdentity(authVersion);
  const operator = Boolean(identity && identity.operator === true);
  const pulse = useIngestPulse(identity, authVersion);

  const changeToken = (token) => {
    setToken(token);
    setTokenDraft('');
    setTokenModal(false);
    authFailed.current = false;
    setAuthVersion((v) => v + 1);
  };

  const [head, second, third] = route.parts;
  const screen = head || 'overview';
  const go = React.useCallback((parts, params) => navigate(Array.isArray(parts) ? parts : [parts], params), []);

  // Leaving a screen closes the narrow-width drawer: it was opened to go
  // somewhere, and that has now happened.
  React.useEffect(() => { setDrawer(false); }, [route]);

  useKeys((event, { typing }) => {
    const meta = event.metaKey || event.ctrlKey;
    if (meta && event.key.toLowerCase() === 'k') {
      event.preventDefault();
      setPalette((open) => !open);
      return;
    }
    if (typing || meta || event.altKey) return;
    if (event.key === '?') { event.preventDefault(); go(['connect']); }
    if (event.key === 'Escape' && drawer) setDrawer(false);
  }, [go, drawer]);

  const common = { pushToast, go, params: route.params, authVersion, identity };
  const k = (name) => name + ':' + authVersion;
  let view;
  if (OPERATOR_SCREENS.has(screen) && !operator) {
    // Not offered in the rail, but a pasted link still lands here. Say why
    // rather than asking the server a question this credential may not ask.
    view = identity == null
      ? <Skeleton height={120} />
      : <EmptyState message={<span>
        {SCREENS[screen][0]} reads store-global surfaces, so it is open to unbound credentials only.
        {identity.tenant ? <> This credential is bound to tenant <code>{identity.tenant}</code>.</> : null}
        {' '}Set an unbound token to open it.
      </span>} />;
  } else {
    switch (screen) {
      case 'traces': view = <TracesScreen key={k('traces')} {...common} />; break;
      case 'trace': view = <TraceScreen key={k('trace:' + second)} traceId={second} {...common} />; break;
      case 'sessions':
        view = second
          ? <SessionScreen key={k('session:' + second)} sessionId={second} {...common} />
          : <SessionsScreen key={k('sessions')} {...common} />;
        break;
      case 'conversation':
        view = <ConversationScreen key={k('conv:' + second + ':' + third)} kind={second} id={third} {...common} />;
        break;
      case 'run': view = <RunScreen key={k('run:' + second)} runId={second} {...common} />; break;
      case 'guard': view = <GuardScreen key={k('guard')} {...common} />; break;
      case 'diff': view = <DiffScreen key={k('diff')} {...common} />; break;
      case 'analytics': view = <AnalyticsScreen key={k('analytics')} {...common} />; break;
      case 'latency': view = <LatencyScreen key={k('latency')} {...common} />; break;
      case 'failures': view = <FailuresScreen key={k('failures')} {...common} />; break;
      case 'scores': view = <ScoresScreen key={k('scores')} {...common} />; break;
      case 'experiments': view = <ExperimentsScreen key={k('experiments')} {...common} />; break;
      case 'datasets': view = <DatasetsScreen key={k('datasets')} {...common} />; break;
      case 'tail': view = <TailScreen key={k('tail')} {...common} />; break;
      case 'compare': view = <CompareScreen key={k('compare')} {...common} />; break;
      case 'server': view = <ServerScreen key={k('server')} {...common} />; break;
      case 'store': view = <StoreScreen key={k('store')} {...common} />; break;
      case 'ledger': view = <LedgerScreen key={k('ledger')} {...common} />; break;
      case 'credentials': view = <CredentialsScreen key={k('credentials')} {...common} onUseToken={changeToken} />; break;
      case 'connect': view = <ConnectScreen key={k('connect')} {...common} />; break;
      case 'mcp': view = <McpScreen key={k('mcp')} {...common} />; break;
      default: view = <OverviewScreen key={k('overview')} {...common} />;
    }
  }

  // Detail screens render under their section, so the rail still shows
  // where you are.
  const railScreen = screen === 'trace' ? 'traces'
    : screen === 'conversation' ? (second === 'sessions' ? 'sessions' : 'traces')
      : screen === 'compare' ? 'traces'
        : screen === 'run' ? 'sessions' : screen;

  const subtitle = screen === 'trace' && second ? second
    : screen === 'run' && second ? second
      : screen === 'diff' && route.params.get('a') && route.params.get('b')
        ? `${route.params.get('a')} against ${route.params.get('b')}` : undefined;

  return <div style={{
    display: 'flex', minHeight: '100vh', background: 'var(--bg)', color: 'var(--ink)',
    fontFamily: 'var(--font-sans)', fontSize: 13,
  }}>
    <NavRail screen={railScreen} ingest={pulse} identity={identity} open={drawer} onGo={(id) => go([id])} />
    <div className="tz-scrim" data-open={drawer ? 'true' : 'false'} onClick={() => setDrawer(false)} aria-hidden="true" />
    <div style={{ flex: 1, minWidth: 0, display: 'flex', flexDirection: 'column' }}>
      <Header screen={SCREENS[screen] ? screen : 'overview'} subtitle={subtitle}
        count={pulse.count} density={density} onDensity={setDensity}
        theme={theme} onTheme={() => setThemeChoice(theme === 'dark' ? 'light' : 'dark')}
        onPalette={() => setPalette(true)} onToken={() => setTokenModal(true)}
        onMenu={() => setDrawer((open) => !open)} menuOpen={drawer} />
      <main className="tz-main" style={{ flex: 1, minWidth: 0, padding: '16px 20px 64px' }}>{view}</main>
    </div>

    <CommandPalette open={palette} onClose={() => setPalette(false)} nav={navFor(operator)}
      onNavigate={(parts) => go(parts)} />

    <Modal open={tokenModal} title="Set token" onClose={() => setTokenModal(false)} footer={<>
      {getToken() ? <Button onClick={() => changeToken('')}>Clear token</Button> : null}
      <Button variant="primary" onClick={() => changeToken(tokenDraft.trim())} disabled={!tokenDraft.trim()}>Set token</Button>
    </>}>
      <div style={{ display: 'grid', gap: 8 }}>
        <div style={{ color: 'var(--ink-muted)' }}>
          The API is gated once any credential exists (<code>TRAZA_TOKENS</code>, or one minted at
          runtime). The token stays in this tab's session storage and is sent as a bearer header.
          {identity && !identity.unauthorized && identity.auth !== 'unknown'
            ? <> Current: {identity.auth === 'open' ? 'open store, no token needed' : <>
              <code>{identity.scope}</code>{identity.tenant ? <> bound to <code>{identity.tenant}</code></> : ' unbound'}
              {identity.credential?.label ? <> ({identity.credential.label})</> : null}</>}.</>
            : null}
        </div>
        <Input mono type="password" placeholder="token" value={tokenDraft} onChange={setTokenDraft}
          autoComplete="off"
          onKeyDown={(e) => { if (e.key === 'Enter' && tokenDraft.trim()) changeToken(tokenDraft.trim()); }} />
      </div>
    </Modal>

    <div style={{ position: 'fixed', right: 16, bottom: 16, display: 'grid', gap: 8, zIndex: 95 }}>
      {toasts.map((t) => <Toast key={t.id} status={t.status} title={t.title} detail={t.detail}
        onDismiss={() => setToasts((list) => list.filter((x) => x.id !== t.id))} />)}
    </div>
  </div>;
}
