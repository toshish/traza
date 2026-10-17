// @vitest-environment jsdom
//
// The API client's promises, tested at the one place they are kept: the URL
// every request is sent to (the API base), what happens after a 401 (nothing,
// until a token is set), what a non-operator credential may ask (never a
// store-global surface), and how a 403 is phrased (with the capability the
// server said was missing).

import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { api, setToken } from './api.js';
import { apiBase, apiUrl, publicBase } from './base.js';
import { isAuthBlocked, isOperatorPath, setIdentity } from './session.js';

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
