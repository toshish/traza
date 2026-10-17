// @vitest-environment jsdom
//
// The whole app against a fake server, for the promises that only exist at
// the app level: which surfaces it polls depends on who the server says the
// credential is, and a 401 silences every poll on the page.
//
// The fake answers from fixtures captured from a real traza-server 0.27.0
// (src/test/fixtures); routes the screens read but these tests do not assert
// on get small empty bodies of the right shape.

import React from 'react';
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, act, cleanup, screen } from '@testing-library/react';
import { App } from './App.jsx';
import { getToken, setToken } from './lib/api.js';
import { setIdentity } from './lib/session.js';
import { isOperatorPath } from './lib/session.js';
import whoamiBound from './test/fixtures/whoami_bound.json';
import whoamiOpen from './test/fixtures/whoami_open.json';
import usageBound from './test/fixtures/usage_bound.json';

const json = (status, body) => new Response(JSON.stringify(body), {
  status, headers: { 'Content-Type': 'application/json' },
});

const EMPTY = {
  '/v1/stats/series': { buckets: [], bucket_ns: 1, since_ns: 0 },
  '/v1/stats/failures': { groups: [], total: 0 },
  '/v1/stats/llm': { rows: [] },
  '/v1/stats/duration': { buckets: [], p50_ns: 0, p95_ns: 0, p99_ns: 0 },
  '/v1/sessions': { sessions: [] },
  '/v1/stats': { record_count: 32, durability: 'wal' },
  '/v1/metrics.json': {
    uptime_ns: 1, durability: 'wal', ingest: { spans_admitted: 32 }, requests: { total: 1, p95_ns: 1 },
    by_class: { search: {}, lookup: {}, stats: {}, ingest: {}, other: {} },
  },
};

/** A fake server: `whoami` decides the credential. Records every path. */
function serve({ whoami, whoamiStatus = 200 }) {
  const paths = [];
  globalThis.fetch = vi.fn(async (url) => {
    const path = String(url).split('?')[0];
    paths.push(String(url));
    if (path === '/v1/whoami') return json(whoamiStatus, whoamiStatus === 200 ? whoami : { error: 'unauthorized' });
    if (whoamiStatus === 401) return json(401, { error: 'unauthorized' });
    if (path === '/v1/usage') return json(200, usageBound);
    if (whoami && !whoami.operator && isOperatorPath(path)) return json(403, { error: 'forbidden', required: 'unbound' });
    return json(200, EMPTY[path] || {});
  });
  return paths;
}

async function settle(ms = 0) {
  await act(async () => { await vi.advanceTimersByTimeAsync(ms); });
}

beforeEach(() => {
  vi.useFakeTimers();
  // The poll hook stands down in a hidden tab; this DOM is never visible.
  Object.defineProperty(document, 'hidden', { configurable: true, get: () => false });
  window.location.hash = '#/overview';
  setToken('');
  setIdentity(null);
  localStorage.clear();
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

describe('a tenant-bound credential', () => {
  it('never calls an operator surface, and polls its own usage instead', async () => {
    const paths = serve({ whoami: whoamiBound });
    render(<App />);
    await settle(0);
    await settle(21000);

    expect(paths.filter((p) => isOperatorPath(p))).toEqual([]);
    expect(paths.filter((p) => p.startsWith('/v1/usage')).length).toBeGreaterThanOrEqual(4);
    expect(paths.indexOf('/v1/whoami')).toBeGreaterThanOrEqual(0);
  });

  it('is not offered the operator screens, and the rail names its tenant', async () => {
    serve({ whoami: whoamiBound });
    render(<App />);
    await settle(0);
    await settle(6000);

    const nav = Array.from(document.querySelectorAll('nav[aria-label="Screens"] button')).map((b) => b.textContent);
    expect(nav).toContain('Guard');
    expect(nav).toContain('Ledger');
    expect(nav).not.toContain('Server');
    expect(nav).not.toContain('Store');
    expect(nav).not.toContain('Credentials');
    expect(screen.getByTestId('rail-scope').textContent).toContain('tenant acme');
    expect(document.querySelector('header').textContent).toContain('8 spans sent');
  });

  it('opening an operator screen by link explains instead of asking the server', async () => {
    const paths = serve({ whoami: whoamiBound });
    window.location.hash = '#/server';
    render(<App />);
    await settle(0);
    await settle(100);
    expect(document.querySelector('main').textContent).toMatch(/open to unbound credentials only/);
    expect(paths.filter((p) => isOperatorPath(p))).toEqual([]);
  });
});

describe('an operator', () => {
  it('polls the store-global metrics and sees every screen', async () => {
    const paths = serve({ whoami: whoamiOpen });
    render(<App />);
    await settle(0);
    await settle(6000);
    expect(paths.some((p) => p === '/v1/metrics.json')).toBe(true);
    expect(paths.some((p) => p === '/v1/stats')).toBe(true);
    const nav = Array.from(document.querySelectorAll('nav[aria-label="Screens"] button')).map((b) => b.textContent);
    expect(nav).toEqual(expect.arrayContaining(['Server', 'Store', 'Credentials', 'Ledger', 'Guard', 'Diff']));
    expect(screen.getByTestId('rail-scope').textContent).toContain('open store');
  });
});

describe('a 401', () => {
  it('stops every poll until a token is set', async () => {
    const paths = serve({ whoamiStatus: 401 });
    render(<App />);
    await settle(0);
    await settle(100);
    const before = paths.length;
    await settle(30000);
    expect(paths.length).toBe(before);
    expect(screen.getByRole('dialog', { name: 'Set token' })).toBeTruthy();
  });
});

describe('a token handed off by navigating the open tab', () => {
  /** A server whose whoami answers by the bearer token: bound with one, an
      open store without. */
  function serveByToken() {
    const whoamiAuth = [];
    globalThis.fetch = vi.fn(async (url, init = {}) => {
      const path = String(url).split('?')[0];
      const auth = init.headers && init.headers.Authorization;
      if (path === '/v1/whoami') {
        whoamiAuth.push(auth);
        return json(200, auth ? whoamiBound : whoamiOpen);
      }
      if (path === '/v1/usage') return json(200, usageBound);
      return json(200, EMPTY[path] || {});
    });
    return whoamiAuth;
  }

  async function navigateTo(hash) {
    await act(async () => {
      window.location.hash = hash;
      // jsdom queues its own hashchange; dispatching one as well makes the
      // test independent of when that runs. Taking a token is idempotent.
      window.dispatchEvent(new HashChangeEvent('hashchange'));
    });
    await settle(10);
  }

  it('takes a route-form token, strips it, keeps the route, and asks who it is', async () => {
    const whoamiAuth = serveByToken();
    render(<App />);
    await settle(0);
    await navigateTo('#/guard?token=trz_handed&state=stop');
    expect(getToken()).toBe('trz_handed');
    expect(window.location.hash).toBe('#/guard?state=stop');
    expect(whoamiAuth).toEqual([undefined, 'Bearer trz_handed']);
    expect(screen.getByTestId('rail-scope').textContent).toContain('tenant acme');
  });

  it('takes a whole-fragment token and opens the default screen', async () => {
    serveByToken();
    render(<App />);
    await settle(0);
    await navigateTo('#token=trz_whole');
    expect(getToken()).toBe('trz_whole');
    expect(window.location.hash).not.toContain('trz_whole');
    expect(document.querySelector('header').textContent).toContain('Overview');
  });
});

describe('a whoami that fails for a reason other than 401', () => {
  function serveFlaky(failures) {
    let calls = 0;
    const paths = [];
    globalThis.fetch = vi.fn(async (url) => {
      const path = String(url).split('?')[0];
      paths.push(path);
      if (path === '/v1/whoami') {
        calls += 1;
        return calls <= failures ? json(503, { error: 'busy' }) : json(200, whoamiOpen);
      }
      if (path === '/v1/usage') return json(200, usageBound);
      return json(200, EMPTY[path] || {});
    });
    return { paths, calls: () => calls };
  }

  const nav = () => Array.from(document.querySelectorAll('nav[aria-label="Screens"] button')).map((b) => b.textContent);

  it('is asked again, and the operator gets its screens back once it answers', async () => {
    const server = serveFlaky(1);
    render(<App />);
    await settle(0);
    expect(nav()).not.toContain('Server');
    await settle(2500);
    expect(server.calls()).toBe(2);
    expect(nav()).toContain('Server');
    await settle(6000);
    expect(server.paths).toContain('/v1/metrics.json');
  });

  it('backs off while it keeps failing, rather than giving up or hammering', async () => {
    const server = serveFlaky(Infinity);
    render(<App />);
    await settle(0);
    await settle(30000);
    // 0 s, then 2, 5 and 15 s apart: four asks in the first 30 seconds.
    expect(server.calls()).toBe(4);
    expect(nav()).not.toContain('Server');
  });
});
