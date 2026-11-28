// @vitest-environment jsdom
//
// The API client's promises, tested at the one place they are kept: the URL
// every request is sent to (the API base), what happens after a 401 (nothing,
// until a token is set), what a non-operator credential may ask (never a
// store-global surface), and how a 403 is phrased (with the capability the
// server said was missing).

import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { api, onUnauthorized, setToken } from './api.js';
import { apiBase, apiUrl, publicBase } from './base.js';
import { credentialGeneration, getIdentity, isAuthBlocked, isOperatorPath, setIdentity } from './session.js';

function setBase(content) {
  let meta = document.querySelector('meta[name="traza-api-base"]');
  if (content == null) { meta?.remove(); return; }
  if (!meta) {
    meta = document.createElement('meta');
    meta.setAttribute('name', 'traza-api-base');
    document.head.appendChild(meta);
  }
  meta.setAttribute('content', content);
}

const json = (status, body, headers = {}) => new Response(JSON.stringify(body), {
  status, headers: { 'Content-Type': 'application/json', ...headers },
});

/** Replaces fetch; returns the list of calls it records. */
function mockFetch(handler = () => json(200, {})) {
  const calls = [];
  globalThis.fetch = vi.fn(async (url, init = {}) => {
    calls.push({ url: String(url), init });
    return handler(String(url), init);
  });
  return calls;
}

beforeEach(() => {
  setBase(null);
  setIdentity(null);
  setToken('');
  sessionStorage.clear();
});

afterEach(() => {
  setBase(null);
  setIdentity(null);
  setToken('');
});

describe('the API base', () => {
  it('is empty by default, so requests stay same-origin', async () => {
    const calls = mockFetch(() => json(200, { auth: 'open' }));
    expect(apiBase()).toBe('');
    await api.whoami();
    expect(calls[0].url).toBe('/v1/whoami');
    expect(publicBase()).toBe(window.location.origin);
  });

  it('prefixes every request, stream, payload and MCP call', async () => {
    setBase('/x/org/project/');
    const calls = mockFetch((url) => (url.includes('/v1/tail')
      ? new Response('', { status: 200 })
      : url.includes('/v1/payloads/') ? new Response('bytes', { status: 200 })
        : json(200, { spans: [], result: {} })));
    await api.spans({ limit: 1 });
    await api.tape('trace:abc', { max_steps: 5 });
    await api.payload('sha256/00');
    await api.tailChunks({});
    await api.mcp('tools/list');
    await api.createReplay({ run: 'r' });
    expect(calls.map((c) => c.url)).toEqual([
      '/x/org/project/v1/spans?limit=1',
      '/x/org/project/v1/tapes/trace%3Aabc?max_steps=5',
      '/x/org/project/v1/payloads/sha256%2F00',
      '/x/org/project/v1/tail',
      '/x/org/project/v1/mcp',
      '/x/org/project/v1/replays',
    ]);
  });

  it('names the prefixed base in copyable snippets, and keeps an absolute one as given', () => {
    setBase('/x/org/project');
    expect(apiUrl('/v1/spans')).toBe('/x/org/project/v1/spans');
    expect(publicBase()).toBe(window.location.origin + '/x/org/project');
    setBase('https://api.example.com/store/');
    expect(publicBase()).toBe('https://api.example.com/store');
  });
});

describe('after a 401', () => {
  it('sends nothing more until a token is set, then sends it', async () => {
    let status = 401;
    const calls = mockFetch(() => (status === 401 ? json(401, { error: 'unauthorized' }) : json(200, { auth: 'token' })));

    await expect(api.whoami()).rejects.toMatchObject({ status: 401 });
    expect(isAuthBlocked()).toBe(true);
    await expect(api.spans({ limit: 1 })).rejects.toMatchObject({ status: 401 });
    await expect(api.mcp('tools/list')).rejects.toMatchObject({ status: 401 });
    expect(calls).toHaveLength(1);

    status = 200;
    setToken('trz_test_token');
    expect(isAuthBlocked()).toBe(false);
    await api.whoami();
    expect(calls).toHaveLength(2);
    expect(calls[1].init.headers.Authorization).toBe('Bearer trz_test_token');
  });
});

describe('a 401 for an older token', () => {
  it('does not block the token set since, nor open the prompt', async () => {
    // The open-store bootstrap: polls sent with no token are still in flight
    // when the tab adopts the credential it just minted.
    const releases = [];
    mockFetch(() => new Promise((resolve) => { releases.push(() => resolve(json(401, { error: 'unauthorized' }))); }));
    setIdentity({ auth: 'open', operator: true, scope: 'admin', tenant: null });
    const prompted = vi.fn();
    onUnauthorized(prompted);
    const stale = api.spans({ limit: 1 }).catch((e) => e);
    const staleMcp = api.mcp('tools/list').catch((e) => e);
    await Promise.resolve();
    setToken('trz_fresh');
    releases.forEach((release) => release());
    expect((await stale).status).toBe(401);
    expect((await staleMcp).status).toBe(401);
    expect(isAuthBlocked()).toBe(false);
    expect(prompted).not.toHaveBeenCalled();
    onUnauthorized(null);
  });

  it('still blocks when the token refused is the one in use', async () => {
    mockFetch(() => json(401, { error: 'unauthorized' }));
    const prompted = vi.fn();
    onUnauthorized(prompted);
    setToken('trz_revoked');
    await expect(api.spans({ limit: 1 })).rejects.toMatchObject({ status: 401 });
    expect(isAuthBlocked()).toBe(true);
    expect(prompted).toHaveBeenCalledTimes(1);
    onUnauthorized(null);
  });

  it('does not let a read made with a new token join one sent with the old', async () => {
    const releases = [];
    const calls = mockFetch(() => new Promise((resolve) => { releases.push(() => resolve(json(200, { spans: [] }))); }));
    setIdentity({ auth: 'open', operator: true, scope: 'admin', tenant: null });
    const first = api.spans({ limit: 1 });
    setToken('trz_new');
    const second = api.spans({ limit: 1 });
    await Promise.resolve();
    const seen = calls.map((c) => c.init.headers.Authorization);
    // Settled before asserting, so a failure here cannot leave a request in
    // flight for the next test to join.
    releases.forEach((release) => release());
    await Promise.all([first, second]);
    expect(seen).toEqual([undefined, 'Bearer trz_new']);
  });
});

describe('a token change', () => {
  it('forgets the identity in the same step, so an operator surface waits for the new whoami', async () => {
    // The old answer said "operator" about the old token. Until the server
    // has described the new one, nothing may be sent on its strength.
    setIdentity({ auth: 'open', operator: true, scope: 'admin', tenant: null });
    const calls = mockFetch(() => json(200, { credentials: [] }));
    setToken('trz_tenant');
    expect(getIdentity()).toBeNull();
    await expect(api.credentials()).rejects.toMatchObject({ status: 403, required: 'unbound' });
    await expect(api.metrics()).rejects.toMatchObject({ status: 403, required: 'unbound' });
    expect(calls).toHaveLength(0);
  });

  it('does not take an answer about the token it replaced as the identity', async () => {
    const asked = credentialGeneration();
    setToken('trz_next');
    expect(setIdentity({ auth: 'token', operator: true, scope: 'admin', tenant: null }, asked)).toBe(false);
    expect(getIdentity()).toBeNull();
    const calls = mockFetch(() => json(200, {}));
    await expect(api.metrics()).rejects.toMatchObject({ status: 403 });
    expect(calls).toHaveLength(0);
    // An answer about the token in use is the identity.
    expect(setIdentity({ auth: 'token', operator: true, scope: 'admin', tenant: null }, credentialGeneration())).toBe(true);
    await api.metrics();
    expect(calls.map((c) => c.url)).toEqual(['/v1/metrics.json']);
  });
});

describe('the tenant parameter', () => {
  it('is sent even when empty, because `tenant=` names the default tenant', async () => {
    setIdentity({ auth: 'open', operator: true, scope: 'admin', tenant: null });
    const calls = mockFetch(() => json(200, { runs: [], steps: [], aligned: [] }));
    await api.tape('weather-7', { tenant: '', max_steps: 5 });
    await api.diff({ a: 'x', b: 'y', tenant: '' });
    await api.fuseRun('weather-7', { tenant: '' });
    await api.tape('weather-7', { tenant: undefined, max_steps: 5 });
    await api.spans({ name: '', limit: 1 });
    expect(calls.map((c) => c.url)).toEqual([
      '/v1/tapes/weather-7?tenant=&max_steps=5',
      '/v1/diff?a=x&b=y&tenant=',
      '/v1/fuses/runs/weather-7?tenant=',
      '/v1/tapes/weather-7?max_steps=5',
      '/v1/spans?limit=1',
    ]);
  });
});

describe('a non-operator credential', () => {
  it('never asks a store-global surface, and still reads its own', async () => {
    setIdentity({ auth: 'token', operator: false, scope: 'ro', tenant: 'acme' });
    const calls = mockFetch(() => json(200, { tenants: [], rows: [] }));
    for (const call of [api.stats, api.metrics, api.flush, api.ledgerVerify, api.credentials,
      () => api.ledger({ limit: 1 })]) {
      await expect(call()).rejects.toMatchObject({ status: 403, required: 'unbound' });
    }
    expect(calls).toHaveLength(0);

    await api.usage();
    await api.llmStats({ group_by: 'model' });
    await api.ledgerHead();
    expect(calls.map((c) => c.url)).toEqual(['/v1/usage', '/v1/stats/llm?group_by=model', '/v1/ledger/head']);
  });

  it('includes a credential nobody has described yet', async () => {
    // Before /v1/whoami answers, the page does not know; unknown is no.
    const calls = mockFetch(() => json(200, {}));
    await expect(api.metrics()).rejects.toMatchObject({ status: 403 });
    expect(calls).toHaveLength(0);
    setIdentity({ auth: 'open', operator: true, scope: 'admin', tenant: null });
    await api.metrics();
    expect(calls.map((c) => c.url)).toEqual(['/v1/metrics.json']);
  });

  it('knows exactly which paths are operator surfaces', () => {
    for (const path of ['/v1/stats', '/v1/metrics', '/v1/metrics.json', '/v1/verify', '/v1/flush',
      '/v1/checkpoint', '/v1/backups/nightly', '/v1/admin/credentials', '/v1/admin/x/y',
      '/v1/ledger', '/v1/ledger?after=3', '/v1/ledger/verify', '/v1/ledger/anchors']) {
      expect(isOperatorPath(path), path).toBe(true);
    }
    for (const path of ['/v1/stats/llm', '/v1/stats/series?x=1', '/v1/usage', '/v1/whoami',
      '/v1/ledger/head', '/v1/evidence', '/v1/fuses/runs', '/v1/spans', '/v1/adminx']) {
      expect(isOperatorPath(path), path).toBe(false);
    }
  });
});

describe('a 403', () => {
  it("says which capability the server required", async () => {
    mockFetch(() => json(403, { error: 'forbidden', required: 'rw' }));
    const error = await api.savePolicy({ name: 'p' }).catch((e) => e);
    expect(error.status).toBe(403);
    expect(error.required).toBe('rw');
    expect(error.what).toMatch(/read-only/);
    expect(error.next).toMatch(/requires "rw"/);
  });

  it('tells a foreign-tenant refusal apart from an operator surface', async () => {
    mockFetch(() => json(403, { error: 'this credential is bound to a different tenant', required: 'unbound' }));
    const error = await api.usage({ tenant: 'globex' }).catch((e) => e);
    expect(error.what).toBe('This credential is bound to a different tenant.');
    expect(error.next).toMatch(/requires "unbound"/);
  });
});

describe('the evidence pack', () => {
  it('names the default tenant explicitly and returns the exact bytes', async () => {
    // A timestamp above 2^53: a JSON.parse round trip would not keep it.
    const body = '{"digest":"ab","generated_at_ns":1792223274318980123,"tenant":""}';
    const calls = mockFetch(() => new Response(body, { status: 200 }));
    expect(await api.evidenceText({ tenant: '' })).toBe(body);
    expect(calls[0].url).toBe('/v1/evidence?tenant=');
    await api.evidenceText({ since: '1', until: '2' });
    expect(calls[1].url).toBe('/v1/evidence?since=1&until=2');
  });
});

describe('a stream that ends in an error', () => {
  it('lets the body go without leaving the cancel\'s rejection unhandled, as leaving a live screen ends one', async () => {
    const unhandled = [];
    const onNodeUnhandled = (reason) => unhandled.push(reason);
    process.on('unhandledRejection', onNodeUnhandled);
    try {
      const cut = new DOMException('signal is aborted without reason', 'AbortError');
      mockFetch(() => new Response(new ReadableStream({
        start(controller) {
          controller.enqueue(new TextEncoder().encode('event: snapshot\ndata: {"runs":[]}\n\n'));
          // The connection is cut under the reader, as an abort cuts it.
          setTimeout(() => controller.error(cut), 0);
        },
      }), { status: 200, headers: { 'Content-Type': 'text/event-stream' } }));
      const chunks = await api.liveChunks({});
      const seen = [];
      await expect((async () => { for await (const chunk of chunks) seen.push(chunk); })()).rejects.toBe(cut);
      expect(seen).toHaveLength(1);
      await new Promise((done) => setTimeout(done, 20));
      expect(unhandled).toEqual([]);
    } finally {
      process.off('unhandledRejection', onNodeUnhandled);
    }
  });
});
