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
import { render, act, cleanup, fireEvent, screen, within } from '@testing-library/react';
import { App } from './App.jsx';
import { getToken, setToken } from './lib/api.js';
import { setIdentity } from './lib/session.js';
import { isOperatorPath } from './lib/session.js';
import whoamiBound from './test/fixtures/whoami_bound.json';
import whoamiOpen from './test/fixtures/whoami_open.json';
import usageBound from './test/fixtures/usage_bound.json';
import liveRuns from './test/fixtures/live_runs.json';
import liveStream from './test/fixtures/live_stream.txt?raw';

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

describe('the landing screen', () => {
  /** An open store whose guard holds `runs` live; the stream answers with
      the captured bytes. Records every path. */
  function serveLive(runs) {
    const paths = [];
    globalThis.fetch = vi.fn(async (url) => {
      const path = String(url).split('?')[0];
      paths.push(String(url));
      if (path === '/v1/whoami') return json(200, whoamiOpen);
      if (path === '/v1/runs/live') return json(200, { ...liveRuns, runs });
      if (path === '/v1/runs/live/stream') {
        return new Response(runs.length ? liveStream : 'event: snapshot\ndata: {"runs":[],"truncated":false,"window_ns":900000000000}\n\n',
          { status: 200, headers: { 'Content-Type': 'text/event-stream' } });
      }
      return json(200, EMPTY[path] || {});
    });
    return paths;
  }

  it('is Live when runs are working right now, decided from one read of the guard', async () => {
    vi.setSystemTime(new Date(Math.max(...liveRuns.runs.map((r) => r.last_activity_ns)) / 1e6 + 1000));
    const paths = serveLive(liveRuns.runs);
    window.location.hash = '';
    render(<App />);
    await settle(0);
    await settle(50);
    expect(paths).toContain('/v1/runs/live?limit=1');
    expect(document.querySelector('header').textContent).toContain('Live');
    expect(screen.getAllByTestId('live-row').length).toBe(liveRuns.runs.length);
    expect(document.querySelector('nav [aria-current="page"]').textContent).toBe('Live');
  });

  it('is the Overview when nothing is live, and an address naming a screen is never redirected', async () => {
    serveLive([]);
    window.location.hash = '';
    render(<App />);
    await settle(0);
    await settle(50);
    expect(document.querySelector('header').textContent).toContain('Overview');
    expect(screen.queryAllByTestId('live-row')).toHaveLength(0);
    cleanup();

    serveLive(liveRuns.runs);
    window.location.hash = '#/overview';
    render(<App />);
    await settle(0);
    await settle(50);
    expect(document.querySelector('header').textContent).toContain('Overview');
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

describe('a credential change', () => {
  // A gated server that answers by bearer token, and holds any request a test
  // asks it to until released. What the page sends while an answer is still
  // outstanding is the point: nothing may be sent with a new token on the
  // strength of the old token's identity, and nothing the old token was
  // answered may land after the change.
  const OPERATOR = { auth: 'token', credential: { id: 'cr_ops', label: 'ops' }, operator: true, scope: 'admin', tenant: null };
  const OPERATOR_B = { ...OPERATOR, credential: { id: 'cr_ops_b', label: 'ops b' } };
  const GLOBEX = { auth: 'token', credential: { id: 'cr_globex', label: 'globex' }, operator: false, scope: 'ro', tenant: 'globex' };
  const usageOf = (tenant, spans) => ({
    as_of_ns: 1, tenants: [{ tenant, lifetime: { batches: 1, bytes: 1, rejected: 0, spans }, limits: {}, month: {}, state: 'ok' }],
  });

  function serveTokens(byToken) {
    const calls = [];
    const held = [];
    const server = {
      calls,
      byToken,
      /** `(call) => boolean`: which requests to hold. */
      hold: () => false,
      /** `(call) => Response | undefined`: a test's own route, asked first. */
      route: () => undefined,
      release(match = () => true) {
        for (let i = 0; i < held.length;) {
          if (match(held[i].call)) held.splice(i, 1)[0].answer();
          else i += 1;
        }
      },
      /** The paths of the calls made with `token`, in order. */
      sentWith: (token) => calls.filter((c) => c.auth === 'Bearer ' + token).map((c) => c.path),
    };
    globalThis.fetch = vi.fn((url, init = {}) => {
      const path = String(url).split('?')[0];
      const call = { path, auth: (init.headers && init.headers.Authorization) || '', method: init.method || 'GET' };
      calls.push(call);
      // Answered when released, from the server's state then: a held reply
      // carries whatever the credential's figures had become by that time.
      const reply = () => {
        const own = server.route(call);
        if (own) return own;
        const who = byToken[call.auth];
        if (!who) return json(401, { error: 'unauthorized' });
        if (path === '/v1/whoami') return json(200, who.whoami);
        if (!who.whoami.operator && isOperatorPath(path)) return json(403, { error: 'forbidden', required: 'unbound' });
        if (who[path]) return json(200, who[path]);
        if (path === '/v1/usage') return json(200, usageBound);
        if (path === '/v1/admin/credentials') return json(200, { credentials: [] });
        return json(200, EMPTY[path] || {});
      };
      if (server.hold(call)) return new Promise((resolve) => held.push({ call, answer: () => resolve(reply()) }));
      return Promise.resolve(reply());
    });
    return server;
  }

  async function navigateTo(hash) {
    await act(async () => {
      window.location.hash = hash;
      window.dispatchEvent(new HashChangeEvent('hashchange'));
    });
    await settle(10);
  }

  const header = () => document.querySelector('header').textContent;

  it('never sends a handed-off tenant token an operator surface, before or after its whoami', async () => {
    // The reproduction: an operator on the dashboard follows a handoff link
    // to an operator screen with a tenant-bound token.
    const server = serveTokens({
      'Bearer op-token': { whoami: OPERATOR },
      'Bearer tenant-token': { whoami: whoamiBound },
    });
    setToken('op-token');
    render(<App />);
    await settle(0);
    await settle(6000);
    expect(server.sentWith('op-token')).toContain('/v1/metrics.json');

    server.hold = (c) => c.path === '/v1/whoami' && c.auth === 'Bearer tenant-token';
    await navigateTo('#/credentials?token=tenant-token');
    await settle(6000);
    expect(server.sentWith('tenant-token')[0]).toBe('/v1/whoami');
    expect(server.sentWith('tenant-token').filter(isOperatorPath)).toEqual([]);

    server.release();
    await settle(6000);
    expect(server.sentWith('tenant-token').filter(isOperatorPath)).toEqual([]);
    expect(server.sentWith('tenant-token')).toContain('/v1/usage');
    expect(document.querySelector('main').textContent).toMatch(/open to unbound credentials only/);
    expect(screen.getByTestId('rail-scope').textContent).toContain('tenant acme');
  });

  it('asks who a new operator token is before sending it any operator surface', async () => {
    // Operator to operator through the prompt: the old identity said
    // "operator", but it said it about another credential.
    const server = serveTokens({
      'Bearer op-token': { whoami: OPERATOR },
      'Bearer op-b': { whoami: OPERATOR_B },
    });
    setToken('op-token');
    render(<App />);
    await settle(0);
    await settle(6000);

    server.hold = (c) => c.path === '/v1/whoami' && c.auth === 'Bearer op-b';
    fireEvent.click(screen.getByText('Set token', { selector: 'header button' }));
    const dialog = screen.getByRole('dialog', { name: 'Set token' });
    fireEvent.change(within(dialog).getByPlaceholderText('token'), { target: { value: 'op-b' } });
    await act(async () => { fireEvent.click(within(dialog).getByText('Set token', { selector: 'button' })); });
    await settle(6000);
    // Tenant-scoped reads may go ahead, as they do on first load; an
    // operator surface waits for the server to say this token is one.
    expect(server.sentWith('op-b')).toContain('/v1/whoami');
    expect(server.sentWith('op-b').filter(isOperatorPath)).toEqual([]);

    server.release();
    await settle(6000);
    expect(server.sentWith('op-b')).toEqual(expect.arrayContaining(['/v1/metrics.json', '/v1/stats']));
  });

  it('drops an operator ingest sample that answers after the switch to a tenant', async () => {
    const server = serveTokens({
      'Bearer op-token': { whoami: OPERATOR },
      'Bearer tenant-token': { whoami: whoamiBound },
    });
    setToken('op-token');
    render(<App />);
    await settle(0);
    expect(header()).toContain('32 records');

    // The next operator sample is in flight when the token changes.
    server.hold = (c) => c.auth === 'Bearer op-token' && (c.path === '/v1/metrics.json' || c.path === '/v1/stats');
    await settle(5000);
    await navigateTo('#/overview?token=tenant-token');
    await settle(10);
    expect(header()).toContain('8 spans sent');

    server.byToken['Bearer op-token']['/v1/stats'] = { record_count: 999, durability: 'wal' };
    server.release();
    await settle(10);
    expect(header()).toContain('8 spans sent');
    expect(header()).not.toContain('999');
    expect(screen.getByTestId('rail-scope').textContent).toContain('tenant acme');
  });

  it("drops one tenant's ingest sample that answers after the switch to another tenant", async () => {
    const server = serveTokens({
      'Bearer tenant-token': { whoami: whoamiBound, '/v1/usage': usageOf('acme', 8) },
      'Bearer globex-token': { whoami: GLOBEX, '/v1/usage': usageOf('globex', 5) },
    });
    setToken('tenant-token');
    render(<App />);
    await settle(0);
    expect(header()).toContain('8 spans sent');

    server.hold = (c) => c.auth === 'Bearer tenant-token' && c.path === '/v1/usage';
    await settle(5000);
    await navigateTo('#/overview?token=globex-token');
    await settle(10);
    expect(header()).toContain('5 spans sent');
    expect(screen.getByTestId('rail-scope').textContent).toContain('tenant globex');

    server.byToken['Bearer tenant-token']['/v1/usage'] = usageOf('acme', 999);
    server.release();
    await settle(10);
    expect(header()).toContain('5 spans sent');
    expect(header()).not.toContain('999');
    expect(screen.getByTestId('rail-scope').textContent).toContain('tenant globex');
  });

  it('adopts the first token minted on an open store, and asks who it is before reading the list with it', async () => {
    const server = serveTokens({ '': { whoami: whoamiOpen } });
    const minted = {
      id: 'cr_first', token: 'trz_first', prefix: 'trz_firs', scope: 'admin', tenant: null,
      label: null, source: 'api', created_at_ns: 1, expires_at_ns: null,
    };
    server.route = (c) => {
      if (c.path !== '/v1/admin/credentials' || c.method !== 'POST') return undefined;
      // Minting closes the store: from here on only the new token works.
      delete server.byToken[''];
      server.byToken['Bearer trz_first'] = { whoami: OPERATOR };
      return json(200, minted);
    };
    window.location.hash = '#/credentials';
    render(<App />);
    await settle(0);
    await settle(10);

    server.hold = (c) => c.path === '/v1/whoami' && c.auth === 'Bearer trz_first';
    await act(async () => { fireEvent.click(screen.getByText('Mint')); });
    await settle(6000);
    expect(getToken()).toBe('trz_first');
    expect(screen.getByTestId('minted-token').textContent).toContain('trz_first');
    expect(server.sentWith('trz_first')[0]).toBe('/v1/whoami');
    expect(server.sentWith('trz_first').filter(isOperatorPath)).toEqual([]);
    expect(screen.queryByRole('dialog', { name: 'Set token' })).toBeNull();

    server.release();
    await settle(10);
    expect(server.sentWith('trz_first')).toContain('/v1/admin/credentials');
    // The one-time display survives the switch, and closing it is all Done
    // does: the token was adopted, and asked about, when it was minted.
    expect(screen.getByTestId('minted-token').textContent).toContain('trz_first');
    const asked = server.sentWith('trz_first').filter((p) => p === '/v1/whoami').length;
    fireEvent.click(screen.getByText('Done'));
    await settle(10);
    expect(screen.queryByTestId('minted-token')).toBeNull();
    expect(server.sentWith('trz_first').filter((p) => p === '/v1/whoami').length).toBe(asked);
    expect(getToken()).toBe('trz_first');
  });

  // An admin credential minted by an operator, whose secret must never be
  // shown to whoever holds the tab after a switch to another credential.
  const ADMIN_MINT = {
    id: 'cr_admin2', token: 'trz_admin_secret', prefix: 'trz_admi', scope: 'admin', tenant: null,
    label: 'second admin', source: 'api', created_at_ns: 1, expires_at_ns: null,
  };

  function serveMint() {
    const server = serveTokens({
      'Bearer op-token': { whoami: OPERATOR },
      'Bearer tenant-token': { whoami: whoamiBound },
    });
    server.route = (c) => (c.path === '/v1/admin/credentials' && c.method === 'POST'
      ? json(200, ADMIN_MINT) : undefined);
    return server;
  }

  it('drops a mint that answers after a switch to another credential, secret unseen', async () => {
    // The reproduction: an operator starts minting an unbound admin
    // credential, then follows a tenant-token handoff before the answer.
    const server = serveMint();
    setToken('op-token');
    window.location.hash = '#/credentials';
    render(<App />);
    await settle(0);
    await settle(10);

    server.hold = (c) => c.path === '/v1/admin/credentials' && c.method === 'POST';
    await act(async () => { fireEvent.click(screen.getByText('Mint')); });
    await settle(10);
    await navigateTo('#/overview?token=tenant-token');
    await settle(10);
    expect(getToken()).toBe('tenant-token');

    server.release((c) => c.method === 'POST');
    await settle(10);
    expect(screen.queryByTestId('minted-token')).toBeNull();
    expect(document.body.textContent).not.toContain('trz_admin_secret');
    expect(screen.queryByRole('dialog', { name: 'Credential minted' })).toBeNull();
  });

  it('never loses the store\'s first admin secret: a mint that closed an open store is shown even after a switch', async () => {
    // An open store takes any token, so a handoff link followed while the
    // first mint is in flight switches the tab before the answer lands —
    // and that answer is the only admin credential the store now has.
    const server = serveTokens({ '': { whoami: whoamiOpen }, 'Bearer handed-off': { whoami: whoamiOpen } });
    const minted = {
      id: 'cr_first', token: 'trz_first', prefix: 'trz_firs', scope: 'admin', tenant: null,
      label: null, source: 'api', created_at_ns: 1, expires_at_ns: null,
    };
    server.route = (c) => {
      if (c.path !== '/v1/admin/credentials' || c.method !== 'POST') return undefined;
      server.byToken = { 'Bearer trz_first': { whoami: OPERATOR } };
      return json(200, minted);
    };
    window.location.hash = '#/credentials';
    render(<App />);
    await settle(0);
    await settle(10);

    server.hold = (c) => c.path === '/v1/admin/credentials' && c.method === 'POST';
    await act(async () => { fireEvent.click(screen.getByText('Mint')); });
    await settle(10);
    await navigateTo('#/overview?token=handed-off');
    expect(getToken()).toBe('handed-off');

    server.release((c) => c.method === 'POST');
    await settle(10);
    const dialog = screen.getByRole('dialog', { name: 'Credential minted' });
    expect(within(dialog).getByTestId('minted-token').textContent).toContain('trz_first');
    expect(dialog.textContent).toMatch(/closed the store/);
    // Not adopted behind the reader's back: the tab keeps the token it was
    // switched to, and the dialog offers this one.
    expect(getToken()).toBe('handed-off');
    expect(within(dialog).getByText('Use in this tab')).toBeTruthy();
  });

  it('clears a minted secret on display when the tab switches to another credential', async () => {
    const server = serveMint();
    setToken('op-token');
    window.location.hash = '#/credentials';
    render(<App />);
    await settle(0);
    await settle(10);

    await act(async () => { fireEvent.click(screen.getByText('Mint')); });
    await settle(10);
    expect(screen.getByTestId('minted-token').textContent).toContain('trz_admin_secret');
    expect(server.calls.some((c) => c.method === 'POST' && c.auth === 'Bearer op-token')).toBe(true);

    await navigateTo('#/overview?token=tenant-token');
    await settle(10);
    expect(screen.queryByTestId('minted-token')).toBeNull();
    expect(document.body.textContent).not.toContain('trz_admin_secret');
  });
});
