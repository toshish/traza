// Hash routing, because the server intentionally serves only "/" and
// "/dashboard". A route is `#/screen/id?params` — and the query a screen is
// showing lives in those params, so a search is a URL somebody else can open.

import React from 'react';

export function parseHash(hash) {
  const raw = (hash || '').replace(/^#\/?/, '');
  const [pathPart, queryPart] = raw.split('?');
  const parts = pathPart.split('/').filter(Boolean).map(decodeURIComponent);
  return { parts, params: new URLSearchParams(queryPart || '') };
}

export function hashFor(parts, params) {
  let hash = '#/' + parts.map(encodeURIComponent).join('/');
  const query = params ? new URLSearchParams(params).toString() : '';
  return query ? hash + '?' + query : hash;
}

/** Push a history entry (a real navigation) or replace the current one.

    Selecting a span inside a trace REPLACES: it is a change of focus, not a
    new place, and pushing it made Back step through every span you clicked
    instead of leaving the trace. Editing a query also replaces — otherwise
    Back would walk one keystroke at a time. */
export function navigate(parts, params, { replace = false } = {}) {
  const hash = hashFor(parts, params);
  if (window.location.hash === hash) return;
  if (replace) {
    window.history.replaceState(null, '', window.location.pathname + window.location.search + hash);
    window.dispatchEvent(new HashChangeEvent('hashchange'));
  } else {
    window.location.hash = hash;
  }
}

/** Browser history if there is somewhere to go back to, else a sensible
    parent route, so Back is never a dead button. */
export function goBack(fallbackParts) {
  if (window.history.length > 1) window.history.back();
  else navigate(fallbackParts || ['overview']);
}

export function useHashRoute() {
  const [route, setRoute] = React.useState(() => parseHash(window.location.hash));
  React.useEffect(() => {
    const onChange = () => setRoute(parseHash(window.location.hash));
    window.addEventListener('hashchange', onChange);
    return () => window.removeEventListener('hashchange', onChange);
  }, []);
  return route;
}

/** Runs an async read, aborting the previous one when its inputs change.

    Every screen here re-reads on a window change, a predicate edit, or a poll
    tick, and without this the superseded request still occupies a connection
    and still resolves — the older answer sometimes landing last and
    overwriting the newer one. The abort makes both problems structural rather
    than something each screen has to remember.

    `deps` are the inputs; `run(signal)` gets the signal to pass to the API. */
export function useRead(run, deps, { skip = false } = {}) {
  const [state, setState] = React.useState({ data: null, error: null, loading: !skip });
  const runRef = React.useRef(run);
  runRef.current = run;
  const [nonce, setNonce] = React.useState(0);

  React.useEffect(() => {
    if (skip) {
      setState({ data: null, error: null, loading: false });
      return undefined;
    }
    const controller = new AbortController();
    let live = true;
    setState((previous) => ({ ...previous, loading: true, error: null }));
    runRef.current(controller.signal).then(
      (data) => { if (live) setState({ data, error: null, loading: false }); },
      (error) => {
        // An abort is this hook superseding itself. Rendering an error for it
        // would flash a failure every time somebody typed.
        if (!live || error?.name === 'AbortError') return;
        setState({ data: null, error, loading: false });
      },
    );
    return () => { live = false; controller.abort(); };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [...deps, nonce, skip]);

  return { ...state, reload: React.useCallback(() => setNonce((n) => n + 1), []) };
}

/** Polls while the tab is visible, and stops when it is not.

    A background tab polling every two seconds is pure waste — nobody is
    looking, and on a laptop it is the difference between an idle radio and a
    busy one. Visibility is the cheapest correct gate. */
export function usePoll(callback, intervalMs, enabled = true) {
  const saved = React.useRef(callback);
  saved.current = callback;
  React.useEffect(() => {
    if (!enabled || !intervalMs) return undefined;
    let timer = null;
    const tick = () => { if (!document.hidden) saved.current(); };
    const start = () => { stop(); timer = setInterval(tick, intervalMs); };
    const stop = () => { if (timer) clearInterval(timer); timer = null; };
    const onVisibility = () => { if (document.hidden) stop(); else { tick(); start(); } };
    start();
    document.addEventListener('visibilitychange', onVisibility);
    return () => { stop(); document.removeEventListener('visibilitychange', onVisibility); };
  }, [intervalMs, enabled]);
}

/** Registers a global key handler that stands down inside text inputs.

    Every shortcut here is a single letter, so firing them while somebody is
    typing a predicate would be unusable. */
export function useKeys(handler, deps = []) {
  React.useEffect(() => {
    const onKey = (event) => {
      const target = event.target;
      const typing = target && (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA'
        || target.tagName === 'SELECT' || target.isContentEditable);
      handler(event, { typing });
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, deps);
}

/** A value that survives reloads. Saved views, density and theme are
    preferences, not state — losing them on refresh is a papercut every
    single time. */
export function useStored(key, initial) {
  const [value, setValue] = React.useState(() => {
    try {
      const raw = localStorage.getItem(key);
      return raw == null ? initial : JSON.parse(raw);
    } catch (e) { return initial; }
  });
  React.useEffect(() => {
    try { localStorage.setItem(key, JSON.stringify(value)); } catch (e) { /* private mode */ }
  }, [key, value]);
  return [value, setValue];
}
