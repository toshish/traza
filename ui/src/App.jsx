import React from 'react';
import { api, getToken, setToken, onUnauthorized } from './lib/api.js';
import { credentialGeneration, onCredentialChange, setIdentity } from './lib/session.js';
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
import { CredentialsScreen, TokenOnce } from './views/CredentialsScreen.jsx';

/** How many samples the rail's ingest sparkline keeps. At a 5s poll this is
    about two minutes of history — long enough to show a shape, short enough
    that a burst three minutes ago is not still claiming the eye. */
const PULSE_SAMPLES = 28;

/** How long to wait before asking whoami again after it failed for a reason
    other than 401: quickly at first (a restart, a moment at the connection
    limit), then at a pace that costs nothing. The last delay repeats. */
const WHOAMI_RETRY_MS = [2000, 5000, 15000];

/** Asks the server what the current credential is, once per credential
    generation — and again, with backoff, until it gets an answer.

    Everything tenant-aware hangs off this answer: which screens are offered,
    which surfaces are polled, what the rail says you are looking at. Until it
    arrives the identity is `null`, and nothing that depends on it runs — a
    bound token must never have asked an operator surface "just while we
    found out". The answer is also handed to the API client, which refuses
    operator paths for a non-operator itself.

    Each answer is kept with the generation it was asked under, and only an
    answer about the current generation is returned. Clearing the old one in
    an effect left a render in between: screens mounted for the new token saw
    the old token's "operator" and asked operator surfaces with the new one.
    Tagged, the render that first sees a new generation already sees `null`.

    A failure that is not a 401 (the server restarting, a 503 at the
    connection limit, a dropped connection) says nothing about the
    credential. It is read as "not an operator" for as long as it lasts, but
    only that long: asking once and keeping the failure would leave an
    operator without its screens, polling the tenant surfaces, until somebody
    happened to change the token. */
function useIdentity(generation) {
  const [answer, setAnswer] = React.useState(null);
  React.useEffect(() => {
    let live = true;
    let timer = null;
    let failures = 0;
    const ask = () => {
      // Read with the request, not taken from the render: the token whoami
      // carries is the one current when it is sent, and the answer is about
      // that token's generation.
      const asked = credentialGeneration();
      api.whoami().then(
        (who) => {
          if (!live || !setIdentity(who, asked)) return;
          setAnswer({ generation: asked, identity: who });
        },
        (error) => {
          if (!live) return;
          if (error && error.status === 401) {
            setAnswer({ generation: asked, identity: { unauthorized: true } });
            return;
          }
          // Unknown is treated as "not an operator": the safe reading of a
          // credential nobody has described is the one that never touches a
          // store-global surface. Set once, so a retry that fails again does
          // not hand every screen a new identity to re-render for.
          if (failures === 0) {
            const unknown = { auth: 'unknown', operator: false, scope: null, tenant: null, error };
            if (setIdentity(unknown, asked)) setAnswer({ generation: asked, identity: unknown });
          }
          timer = setTimeout(ask, WHOAMI_RETRY_MS[Math.min(failures, WHOAMI_RETRY_MS.length - 1)]);
          failures += 1;
        },
      );
    };
    ask();
    return () => { live = false; clearTimeout(timer); };
  }, [generation]);
  return answer && answer.generation === generation ? answer.identity : null;
}

const NO_PULSE = { rate: null, spark: [], live: false, durability: null, count: null };

/** Watches ingest by differencing a counter.

    A rate is not a metric the server keeps — it keeps counters, which is the
    honest thing to keep — so the client differences one. Which counter
    depends on who is asking. An operator reads `/v1/metrics.json` (spans
    admitted by the whole process) and `/v1/stats` (records held and the
    durability mode). A tenant-bound credential may not read either — they
    disclose co-tenants' volume — so it differences its own row of
    `/v1/usage` (spans its tenant has sent), which is also what its header
    count shows. One small read every five seconds either way.

    Every sample belongs to an epoch: one credential generation, read in one
    mode, for one tenant. Resetting the figures on a change was not enough,
    because a sample already in flight still landed afterwards — the header
    of a tenant showing the operator's record count, or another tenant's
    spans. A change now aborts the old epoch's reads, a sample that answers
    anyway is dropped, and the figures carry their epoch, so a render under a
    new one never shows the old one's. */
function useIngestPulse(identity, generation) {
  const mode = !identity || identity.unauthorized ? null : identity.operator ? 'operator' : 'tenant';
  const tenant = (identity && identity.tenant) || '';
  const epoch = `${generation}\u0000${mode}\u0000${tenant}`;
  const [pulse, setPulse] = React.useState(() => ({ epoch, ...NO_PULSE }));
  const sampler = React.useRef(null);

  React.useEffect(() => {
    if (!mode) return undefined;
    const controller = new AbortController();
    const { signal } = controller;
    // The previous reading, kept per epoch: a rate differenced across two
    // credentials' counters would be a number about nobody.
    let previous = null;
    const sample = async () => {
      try {
        let admitted;
        let count;
        let durability = null;
        if (mode === 'operator') {
          const [metrics, stats] = await Promise.all([api.metrics(signal), api.stats(signal)]);
          admitted = metrics?.ingest?.spans_admitted ?? 0;
          durability = stats.durability;
          count = { value: stats.record_count, label: 'records', title: 'Records held by the store (GET /v1/stats)' };
        } else {
          const usage = await api.usage(undefined, signal);
          const rows = usage?.tenants || [];
          const row = rows.find((r) => r.tenant === tenant) || rows[0];
          admitted = row?.lifetime?.spans ?? 0;
          count = {
            value: admitted, label: 'spans sent',
            title: 'Spans this tenant has sent since its counters began (GET /v1/usage)',
          };
        }
        // An answer can win the race with the abort; it is still the old
        // epoch's answer.
        if (signal.aborted) return;
        const now = performance.now();
        const last = previous;
        previous = { admitted, at: now };
        if (!last) {
          setPulse({ epoch, ...NO_PULSE, durability, count });
          return;
        }
        const seconds = Math.max(0.001, (now - last.at) / 1000);
        const rate = Math.max(0, (admitted - last.admitted) / seconds);
        setPulse((p) => ({
          epoch,
          rate,
          live: rate > 0,
          durability,
          count,
          spark: [...(p.epoch === epoch ? p.spark : []), rate].slice(-PULSE_SAMPLES),
        }));
      } catch (e) { /* the rail goes quiet; screens surface their own errors */ }
    };
    sampler.current = sample;
    sample();
    return () => {
      controller.abort();
      if (sampler.current === sample) sampler.current = null;
    };
  }, [epoch, mode, tenant]);
  usePoll(() => { if (sampler.current) sampler.current(); }, 5000, Boolean(mode));
  return pulse.epoch === epoch ? pulse : NO_PULSE;
}

export function App() {
  // A token handed off by fragment navigation is stored by the router hook
  // before the route is parsed; from here it is a token change like any other.
  const route = useHashRoute(() => tokenChanged());
  const [toasts, setToasts] = React.useState([]);
  const [tokenModal, setTokenModal] = React.useState(false);
  const [tokenDraft, setTokenDraft] = React.useState('');
  // The credential generation, read from the store that setToken bumps rather
  // than counted here: whoever changes the token — the prompt, a handoff
  // link, the open-store bootstrap — this render follows it, and none of them
  // has to remember to tell the app.
  const generation = React.useSyncExternalStore(onCredentialChange, credentialGeneration);
  // A credential just minted, shown once. Held here rather than by the
  // Credentials screen: adopting the first token on an open store is a
  // credential change, which remounts every screen, and the one-time display
  // must survive it.
  const [minted, setMinted] = React.useState(null);
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

  const identity = useIdentity(generation);
  const operator = Boolean(identity && identity.operator === true);
  const pulse = useIngestPulse(identity, generation);

  // After a new token is stored: close the prompt and re-arm it. Re-asking
  // whoami and remounting every screen under the token follow from the
  // generation, which setToken has already moved.
  function tokenChanged() {
    setTokenDraft('');
    setTokenModal(false);
    authFailed.current = false;
  }

  const changeToken = (token) => {
    setToken(token);
    tokenChanged();
  };

  // On an open store the mint closed the store: every request from here on
  // needs a token, and the only one that exists is the one just minted. It
  // is adopted at once, or the next poll would answer 401 and open the prompt
  // over the one-time display — and adopting it is a token change like any
  // other: the screens wait for whoami before asking an operator surface.
  const onMinted = (doc, { adopt = false } = {}) => {
    setMinted(adopt ? { ...doc, adopted: true } : doc);
    if (adopt) changeToken(doc.token);
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

  const common = { pushToast, go, params: route.params, identity };
  // Every screen is remounted under a new credential: state built from one
  // token's answers is not carried into another's.
  const k = (name) => name + ':' + generation;
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
      case 'credentials': view = <CredentialsScreen key={k('credentials')} {...common} onMinted={onMinted} />; break;
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

    <TokenOnce doc={minted} onClose={() => setMinted(null)}
      onUse={(token) => { setMinted(null); changeToken(token); }} />

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
