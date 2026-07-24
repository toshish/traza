// API client for the Traza server. The dashboard shell is served without
// credentials; every /v1 call is auth-gated when TRAZA_TOKENS is set, so the
// client attaches a bearer token (sessionStorage only, per the shell's
// contract) and routes 401s to the token prompt.

const TOKEN_KEY = 'traza_token';

export function getToken() {
  try { return sessionStorage.getItem(TOKEN_KEY) || ''; } catch (e) { return ''; }
}

export function setToken(token) {
  try {
    if (token) sessionStorage.setItem(TOKEN_KEY, token);
    else sessionStorage.removeItem(TOKEN_KEY);
  } catch (e) { /* storage unavailable: the token lives for this page only */ }
}

let unauthorizedHandler = null;
export function onUnauthorized(handler) { unauthorizedHandler = handler; }

export class ApiError extends Error {
  constructor(status, what, next) {
    super(what);
    this.status = status;
    this.what = what;
    this.next = next;
  }
}

function authHeaders(extra) {
  const headers = { ...extra };
  const token = getToken();
  if (token) headers['Authorization'] = 'Bearer ' + token;
  return headers;
}

async function request(path, { method = 'GET', body, raw = false } = {}) {
  let response;
  try {
    response = await fetch(path, {
      method,
      headers: authHeaders(body != null ? { 'Content-Type': 'application/json' } : {}),
      body: body != null ? JSON.stringify(body) : undefined,
    });
  } catch (e) {
    throw new ApiError(0, 'The server did not respond.',
      'Check that traza-server is running and reachable, then retry.');
  }
  if (response.status === 401) {
    if (unauthorizedHandler) unauthorizedHandler();
    throw new ApiError(401, 'This request needs a bearer token.',
      'Set a token from TRAZA_TOKENS with "Set token", then retry.');
  }
  if (response.status === 403) {
    throw new ApiError(403, 'The token lacks the scope for this request.',
      'Use a token whose scopes cover it, then retry.');
  }
  if (raw) {
    if (!response.ok) {
      throw new ApiError(response.status, 'Request failed with status ' + response.status + '.', 'Retry; see the server log for detail.');
    }
    return response;
  }
  const data = await response.json().catch(() => null);
  if (!response.ok) {
    const what = (data && data.error) || 'Request failed with status ' + response.status + '.';
    const next = response.status === 503
      ? 'The ingest writer is unavailable; retry with backoff. See /v1/stats.'
      : response.status === 404 ? undefined : 'Retry; see the server log for detail.';
    throw new ApiError(response.status, what, next);
  }
  return data;
}

function query(params) {
  const pairs = Object.entries(params)
    .filter(([, value]) => value !== undefined && value !== null && value !== '')
    .map(([key, value]) => encodeURIComponent(key) + '=' + encodeURIComponent(value));
  return pairs.length ? '?' + pairs.join('&') : '';
}

export const api = {
  stats: () => request('/v1/stats'),
  spans: (filters) => request('/v1/spans' + query(filters)),
  trace: (traceId) => request('/v1/traces/' + encodeURIComponent(traceId)),
  sessions: (params = {}) => request('/v1/sessions' + query(params)),
  session: (sessionId) => request('/v1/sessions/' + encodeURIComponent(sessionId)),
  llmStats: (params = {}) => request('/v1/stats/llm' + query(params)),
  annotations: (params) => request('/v1/annotations' + query(params)),
  annotate: (annotation) => request('/v1/annotations', { method: 'POST', body: annotation }),
  flush: () => request('/v1/flush', { method: 'POST' }),
  payload: async (ref) => {
    const response = await request('/v1/payloads/' + encodeURIComponent(ref), { raw: true });
    return response.text();
  },
  // Streams an export with a hard client-side byte cap, counting rows as
  // they arrive. Browsers cannot read HTTP trailers, so the server's
  // X-Traza-Export-Complete signal is NOT observable here — callers must
  // not claim verified completeness for this path; curl can verify it.
  exportStream: async (filters, { maxBytes = 256 * 1024 * 1024 } = {}) => {
    const response = await request('/v1/export' + query(filters), { raw: true });
    const reader = response.body.getReader();
    const chunks = [];
    let bytes = 0;
    let rows = 0;
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      bytes += value.byteLength;
      if (bytes > maxBytes) {
        reader.cancel();
        throw new ApiError(0,
          'The export passed the in-browser cap of ' + Math.round(maxBytes / (1 << 20)) + ' MiB.',
          'Use the curl command — it streams to disk and can verify the completion trailer.');
      }
      let at = -1;
      while ((at = value.indexOf(10, at + 1)) !== -1) rows += 1;
      chunks.push(value);
    }
    return { blob: new Blob(chunks, { type: 'application/x-ndjson' }), rows, bytes };
  },
  exportPath: (filters) => '/v1/export' + query(filters),
};
