// API client for the Traza server. The dashboard shell is served without
// credentials; every /v1 call is auth-gated when TRAZA_TOKENS is set, so the
// client attaches a bearer token (sessionStorage only, per the shell's
// contract) and routes 401s to the token prompt.
//
// Three things here exist for network economy rather than convenience:
//   - every read takes an AbortSignal, so a superseded query stops occupying
//     a connection the moment its answer stops being wanted;
//   - identical in-flight GETs share one request (`coalesce`), because a
//     screen with five panels on the same window would otherwise ask five
//     times for the same bytes;
//   - paging uses the server's cursor, so "load more" costs one page rather
//     than re-fetching everything already on screen.
//
// Every URL is built through `apiUrl` (see base.js), so the dashboard works
// behind a reverse proxy that mounts the store under a path prefix.

import { apiUrl } from './base.js';
import {
  blockAuth, credentialChanged, getIdentity, isAuthBlocked, isOperatorPath,
} from './session.js';

const TOKEN_KEY = 'traza_token';

export function getToken() {
  try { return sessionStorage.getItem(TOKEN_KEY) || ''; } catch (e) { return ''; }
}

/** Stores (or clears) the bearer token, and in the same step starts a new
    credential generation (see session.js): the identity is forgotten and the
    401 block lifts. Synchronous on purpose — from the moment the token
    changes, an operator surface is refused until the server has said who
    the new token is. */
export function setToken(token) {
  try {
    if (token) sessionStorage.setItem(TOKEN_KEY, token);
    else sessionStorage.removeItem(TOKEN_KEY);
  } catch (e) { /* storage unavailable: the token lives for this page only */ }
  credentialChanged();
}

let unauthorizedHandler = null;
export function onUnauthorized(handler) { unauthorizedHandler = handler; }

export class ApiError extends Error {
  constructor(status, what, next, extra = {}) {
    super(what);
    this.status = status;
    this.what = what;
    this.next = next;
    /** For a 403: the capability the server said was missing (`rw`, `admin`,
        `unbound`), or undefined. */
    this.required = extra.required;
    /** The server's parsed error body, when it sent one. */
    this.body = extra.body;
  }
}

/** The 401 every request answers while the client is blocked, and the one a
    real 401 becomes. One wording, so a screen that failed locally reads the
    same as one the server refused. */
function unauthorized() {
  return new ApiError(401, 'This request needs a bearer token.',
    'Set a token with "Set token", then retry.');
}

/** A 403, phrased from the server's `required` field.

    The server names the capability the credential lacked, precisely so a
    client can say which grant would have worked; "the token lacks the scope"
    without naming it sends people to read server logs for a word the response
    already carried. */
export function forbidden(body) {
  const required = body && typeof body.required === 'string' ? body.required : undefined;
  const said = body && typeof body.error === 'string' ? body.error : '';
  let what;
  if (required === 'rw') what = 'This credential is read-only; this action needs the rw scope.';
  else if (required === 'admin') what = 'This action needs an admin credential.';
  else if (required === 'unbound') {
    what = /different tenant/.test(said)
      ? 'This credential is bound to a different tenant.'
      : 'This is an operator surface; a tenant-bound credential cannot use it.';
  } else what = said || 'The server refused this request.';
  const next = required
    ? `The server requires "${required}" here. Use a credential that has it.`
    : (body && typeof body.next === 'string' ? body.next : undefined);
  return new ApiError(403, what, next, { required, body });
}

/** An abort is a deliberate cancellation, not a failure — callers check this
    to avoid rendering an error state for a query nobody is waiting for. */
export function isAbort(error) {
  return error && (error.name === 'AbortError' || error.aborted === true);
}

function authHeaders(token, extra) {
  const headers = { ...extra };
  if (token) headers['Authorization'] = 'Bearer ' + token;
  return headers;
}

/** What a 401 means depends on the token it refused. Only a refusal of the
    token in use NOW says anything about it: a request sent with an older
    token (or with none) that comes back 401 after a new one was set is old
    news. Blocking on it would refuse a valid token locally and open the
    prompt over whatever the page was showing — on the open-store bootstrap,
    over the one-time display of the credential just minted, because polls
    sent before minting answer 401 after the tab has adopted it. */
function refused(token) {
  if (token !== getToken()) return;
  blockAuth();
  if (unauthorizedHandler) unauthorizedHandler();
}

async function request(path, { method = 'GET', body, raw = false, signal } = {}) {
  // Refused here, before the network, for two reasons that are both promises
  // the dashboard makes: after a 401 nothing is re-sent until a token is set,
  // and only a credential the server has confirmed is an operator asks a
  // store-global surface — even a screen that forgot to check cannot leak the
  // request. Before /v1/whoami has answered for the token in use (on load,
  // and again after every change) the credential is unknown, and unknown
  // counts as no. The identity and the token are read together, with no
  // await between them, so the token sent is the one the identity is about.
  if (isAuthBlocked()) throw unauthorized();
  const identity = getIdentity();
  if (isOperatorPath(path) && !(identity && identity.operator === true)) {
    throw forbidden({ error: 'forbidden', required: 'unbound' });
  }
  const token = getToken();
  let response;
  try {
    response = await fetch(apiUrl(path), {
      method,
      headers: authHeaders(token, body != null ? { 'Content-Type': 'application/json' } : {}),
      body: body != null ? JSON.stringify(body) : undefined,
      signal,
    });
  } catch (e) {
    if (isAbort(e)) throw e;
    throw new ApiError(0, 'The server did not respond.',
      'Check that traza-server is running and reachable, then retry.');
  }
  if (response.status === 401) {
    refused(token);
    throw unauthorized();
  }
  if (response.status === 403) {
    throw forbidden(await response.json().catch(() => null));
  }
  if (raw) {
    if (!response.ok) {
      const data = await response.json().catch(() => null);
      throw new ApiError(response.status, errorText(data) || 'Request failed with status ' + response.status + '.',
        'Retry; see the server log for detail.', { body: data });
    }
    return response;
  }
  const data = await response.json().catch(() => null);
  if (!response.ok) {
    const what = errorText(data) || 'Request failed with status ' + response.status + '.';
    const next = response.status === 503
      ? 'The store could not serve this; retry with backoff.'
      : response.status === 429 ? retryAfterText(response)
        : (response.status === 404 || response.status === 409 || response.status === 400) ? undefined
          : 'Retry; see the server log for detail.';
    throw new ApiError(response.status, what, next, { body: data });
  }
  return data;
}

/** The message in an error body: `{"error": "…"}` on most routes, and
    `{"error": {"message": …}}` in the provider-shaped replay errors. */
function errorText(data) {
  if (!data) return '';
  if (typeof data.error === 'string') return data.error;
  if (data.error && typeof data.error.message === 'string') return data.error.message;
  return '';
}

function retryAfterText(response) {
  const after = response.headers && response.headers.get && response.headers.get('Retry-After');
  return after ? `The server asks for a retry after ${after} seconds.` : 'Retry later.';
}

/** Builds a query string, dropping empties and expanding repeated keys.
    An array value repeats the parameter, which is how the API expresses more
    than one predicate on the same key (`attr.k=a&attr.k=b`).

    `tenant` is the one parameter whose empty value is sent. Across the API
    `tenant=` names the default tenant and leaving it out is the operator
    view across every tenant, so dropping it would turn "this default-tenant
    run" into "every tenant's run of this id". A caller that means "every
    tenant" passes undefined. */
function query(params) {
  const pairs = [];
  for (const [key, value] of Object.entries(params || {})) {
    const list = Array.isArray(value) ? value : [value];
    for (const one of list) {
      if (one === undefined || one === null) continue;
      if (one === '' && key !== 'tenant') continue;
      pairs.push(encodeURIComponent(key) + '=' + encodeURIComponent(one));
    }
  }
  return pairs.length ? '?' + pairs.join('&') : '';
}

// In-flight GETs by token and URL: `{controller, subscribers, promise}`. A
// panel that asks for a window another panel is already fetching joins that
// request instead of opening a second one, and the request is cancelled once
// the last subscriber has gone. The token is part of the key because the
// answer depends on it: a read made after a token change must not join one
// sent with the old token and inherit its 401, or another tenant's rows.
const inFlight = new Map();

/** A coalesced GET whose underlying request is cancelled when — and only
    when — every subscriber has walked away.

    The first attempt gave the shared request no signal at all, so aborting
    rejected the caller's promise while the fetch, the connection and the
    server-side scan all continued. On a screen that re-queries per keystroke
    that is the opposite of the intent: the abort existed precisely to stop
    paying for an answer nobody wants. Reference counting gets both halves —
    one subscriber leaving cancels nothing, the last one leaving cancels the
    work. */
function read(path, signal) {
  const url = getToken() + '\u0000' + path;
  let entry = inFlight.get(url);
  if (!entry) {
    const controller = new AbortController();
    entry = { controller, subscribers: 0, promise: null };
    entry.promise = request(path, { signal: controller.signal })
      // Only ever evict THIS entry. An unconditional `delete(url)` could run
      // after an abort had already replaced the map slot with a fresh request
      // for the same URL, evicting the live one and making the next caller
      // open a third. Identity-checked, so a stale finalizer is a no-op.
      .finally(() => forget(url, entry));
    // Nothing may be attached yet when this settles; without a sink a
    // rejection surfaces as an unhandled promise rejection.
    entry.promise.catch(() => {});
    inFlight.set(url, entry);
  }

  const mine = entry;
  mine.subscribers += 1;
  let released = false;
  const release = () => {
    if (released) return;
    released = true;
    mine.subscribers -= 1;
    if (mine.subscribers <= 0) {
      forget(url, mine);
      mine.controller.abort();
    }
  };

  if (!signal) return mine.promise.finally(release);

  return new Promise((resolve, reject) => {
    if (signal.aborted) {
      release();
      reject(abortError());
      return;
    }
    const onAbort = () => {
      release();
      reject(abortError());
    };
    signal.addEventListener('abort', onAbort, { once: true });
    mine.promise.then(resolve, reject).finally(() => {
      signal.removeEventListener('abort', onAbort);
      release();
    });
  });
}

/** Drops `url` from the in-flight map only if `entry` is still the one there.
    Without the identity check a settling or aborted request can evict its own
    replacement. */
function forget(url, entry) {
  if (inFlight.get(url) === entry) inFlight.delete(url);
}

function abortError() {
  const error = new Error('aborted');
  error.name = 'AbortError';
  return error;
}

export const api = {
  stats: (signal) => read('/v1/stats', signal),

  /** Server and engine instrumentation as JSON. Percentiles carry a stated
      error bound (`percentile_error_bound`); they are bucket upper bounds. */
  metrics: (signal) => read('/v1/metrics.json', signal),

  /** Span search. Returns `{spans, next_cursor, cost}` — `cost` is what the
      query actually touched, which the Traces screen shows rather than
      asserting the store is fast. */
  spans: (filters, signal) => read('/v1/spans' + query(filters), signal),

  trace: (traceId, signal) => read('/v1/traces/' + encodeURIComponent(traceId), signal),
  sessions: (params, signal) => read('/v1/sessions' + query(params), signal),
  /** One session. Session identity is `(tenant, session_id)`: `tenant`
      names whose (empty is the default tenant); without it the operator
      view merges every tenant's session of that id. */
  session: (sessionId, params, signal) => read('/v1/sessions/' + encodeURIComponent(sessionId) + query(params), signal),
  llmStats: (params, signal) => read('/v1/stats/llm' + query(params), signal),

  /** Per-bucket volume, errors, tokens, cost and duration percentiles over a
      window, in one scan. Requires `since` and `until`. */
  series: (params, signal) => read('/v1/stats/series' + query(params), signal),

  /** Duration distribution plus percentiles over a filtered span set. */
  duration: (filters, signal) => read('/v1/stats/duration' + query(filters), signal),

  /** Error spans grouped by `(service, name, status)`, most frequent first. */
  failures: (filters, signal) => read('/v1/stats/failures' + query(filters), signal),

  /** The slowest matching spans — the tail behind a distribution. */
  slowest: (filters, signal) => read('/v1/stats/slowest' + query(filters), signal),

  /** Annotations, across every trace unless one is named. */
  annotations: (params, signal) => read('/v1/annotations' + query(params), signal),

  annotate: (annotation) => request('/v1/annotations', { method: 'POST', body: annotation }),
  flush: () => request('/v1/flush', { method: 'POST' }),
  payload: async (ref, signal) => {
    const response = await request('/v1/payloads/' + encodeURIComponent(ref), { raw: true, signal });
    return response.text();
  },

  // Streams an export with a hard client-side byte cap, counting rows as
  // they arrive. Browsers cannot read HTTP trailers, so the server's
  // X-Traza-Export-Complete signal is NOT observable here — callers must
  // not claim verified completeness for this path; curl can verify it.
  exportStream: async (filters, { maxBytes = 256 * 1024 * 1024, signal } = {}) => {
    const response = await request('/v1/export' + query(filters), { raw: true, signal });
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
  /** The live tail, as an async iterable of decoded text chunks.
   *
   *  Not `EventSource`: that cannot set an `Authorization` header, and the only
   *  alternative it leaves is putting the bearer token in the URL, where it
   *  lands in proxy logs and browser history. `fetch` carries the header, and
   *  its reader is abortable, which `EventSource` also is not.
   *
   *  Decoding is stateful (`stream: true`): a chunk boundary can fall inside a
   *  multi-byte character, and decoding each chunk independently would turn
   *  every such span name into replacement characters. */
  tailChunks: async (filters, signal) => {
    const response = await request('/v1/tail' + query(filters), { raw: true, signal });
    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    return {
      async *[Symbol.asyncIterator]() {
        try {
          for (;;) {
            const { done, value } = await reader.read();
            if (done) break;
            yield decoder.decode(value, { stream: true });
          }
        } finally {
          // Releasing matters on the abort path: without it the response body
          // stays locked and the connection is held until GC gets to it.
          try { reader.cancel(); } catch (e) { /* already closed */ }
        }
      },
    };
  },

  // ---------------------------------------------------------------- identity

  /** What the current credential is: `{auth, scope, tenant, operator,
      credential}`. Not an operator surface; any valid token may ask. */
  whoami: (signal) => request('/v1/whoami', { signal }),

  /** Monotonic ingest counters against limits, per tenant. A bound
      credential gets its own row; this is its accounting surface, and the
      one the rail polls for it instead of the operator metrics. */
  usage: (params, signal) => read('/v1/usage' + query(params), signal),

  // ------------------------------------------------------------- run fuses

  fuseRuns: (params, signal) => read('/v1/fuses/runs' + query(params), signal),
  fuseRun: (run, params, signal) => read('/v1/fuses/runs/' + encodeURIComponent(run) + query(params), signal),
  /** The kill switch. `tenant` selects the run's tenant for an unbound
      credential; a bound one is forced to its own whatever is sent. */
  fuseTrip: (run, reason, params) => request('/v1/fuses/runs/' + encodeURIComponent(run) + '/trip' + query(params),
    { method: 'POST', body: reason ? { reason } : {} }),
  fuseReset: (run, grant, params) => request('/v1/fuses/runs/' + encodeURIComponent(run) + '/reset' + query(params),
    { method: 'POST', body: grant && Object.keys(grant).length ? { grant } : {} }),
  policies: (params, signal) => read('/v1/fuses/policies' + query(params), signal),
  savePolicy: (policy) => request('/v1/fuses/policies', { method: 'POST', body: policy }),
  deletePolicy: (id) => request('/v1/fuses/policies/' + encodeURIComponent(id) + '/delete', { method: 'POST' }),

  // ----------------------------------------------------- tapes and replays

  /** A run's steps in order. `resolve: true` inlines offloaded payloads up
      to the server's per-tape budget. Run ids travel percent-encoded, so a
      `trace:<id>` run or one containing `/` survives the path. */
  tape: (run, params, signal) => read('/v1/tapes/' + encodeURIComponent(run) + query(params), signal),
  createReplay: (body) => request('/v1/replays', { method: 'POST', body }),
  replay: (id, signal) => request('/v1/replays/' + encodeURIComponent(id), { signal }),
  diff: (params, signal) => read('/v1/diff' + query(params), signal),

  // ---------------------------------------------------- ledger and evidence

  ledgerHead: (signal) => read('/v1/ledger/head', signal),
  ledger: (params, signal) => read('/v1/ledger' + query(params), signal),
  ledgerVerify: (signal) => request('/v1/ledger/verify', { signal }),
  /** The evidence pack as its exact bytes. Not parsed and re-serialized: the
      pack's digest is over its canonical JSON, and its timestamps exceed
      2^53, so a JSON.parse round trip would hand an auditor a file whose
      digest no longer checks.

      `tenant` is sent even when empty — `tenant=` names the default tenant,
      and leaving it out asks for the whole store — as `query()` does for
      that parameter everywhere. */
  evidenceText: async (params = {}) => {
    const response = await request('/v1/evidence' + query(params), { raw: true });
    return response.text();
  },

  // ------------------------------------------------------------ credentials

  credentials: (signal) => request('/v1/admin/credentials', { signal }),
  mintCredential: (body) => request('/v1/admin/credentials', { method: 'POST', body }),
  revokeCredential: (id) => request('/v1/admin/credentials/' + encodeURIComponent(id) + '/revoke',
    { method: 'POST', body: {} }),

  exportPath: (filters) => '/v1/export' + query(filters),
  spansPath: (filters) => '/v1/spans' + query(filters),

  /** One JSON-RPC call against the MCP endpoint.
   *
   *  Not routed through `read`: this is a POST, it is not coalescable by URL,
   *  and — the reason it exists at all — a 404 here is a *state to display*
   *  ("MCP is off") rather than an error to raise. The MCP screen shows the
   *  live surface by asking the server for it, so what it lists is what an
   *  agent would actually be offered rather than what this build believes.
   *
   *  Returns `{enabled, result, error}`. A JSON-RPC error is returned, not
   *  thrown, for the same reason: it is the server's answer. */
  mcp: async (method, params, signal) => {
    if (isAuthBlocked()) throw unauthorized();
    const token = getToken();
    let response;
    try {
      response = await fetch(apiUrl('/v1/mcp'), {
        method: 'POST',
        headers: authHeaders(token, {
          'Content-Type': 'application/json',
          'MCP-Protocol-Version': MCP_PROTOCOL_VERSION,
        }),
        body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params: params || {} }),
        signal,
      });
    } catch (e) {
      if (isAbort(e)) throw e;
      throw new ApiError(0, 'The server did not respond.',
        'Check that traza-server is running and reachable, then retry.');
    }
    if (response.status === 404) return { enabled: false };
    if (response.status === 401) {
      refused(token);
      throw unauthorized();
    }
    const data = await response.json().catch(() => null);
    if (!response.ok) {
      throw new ApiError(response.status, (data && data.error) || 'MCP request failed.',
        'See the server log for detail.');
    }
    return { enabled: true, result: data && data.result, error: data && data.error };
  },
};

/** The revision the dashboard speaks. Sent on every MCP call, because the
    server answers 400 to one it does not serve rather than guessing. */
export const MCP_PROTOCOL_VERSION = '2025-11-25';
