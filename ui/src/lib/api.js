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

/** An abort is a deliberate cancellation, not a failure — callers check this
    to avoid rendering an error state for a query nobody is waiting for. */
export function isAbort(error) {
  return error && (error.name === 'AbortError' || error.aborted === true);
}

function authHeaders(extra) {
  const headers = { ...extra };
  const token = getToken();
  if (token) headers['Authorization'] = 'Bearer ' + token;
  return headers;
}

async function request(path, { method = 'GET', body, raw = false, signal } = {}) {
  let response;
  try {
    response = await fetch(path, {
      method,
      headers: authHeaders(body != null ? { 'Content-Type': 'application/json' } : {}),
      body: body != null ? JSON.stringify(body) : undefined,
      signal,
    });
  } catch (e) {
    if (isAbort(e)) throw e;
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

/** Builds a query string, dropping empties and expanding repeated keys.
    An array value repeats the parameter, which is how the API expresses more
    than one predicate on the same key (`attr.k=a&attr.k=b`). */
function query(params) {
  const pairs = [];
  for (const [key, value] of Object.entries(params || {})) {
    if (value === undefined || value === null || value === '') continue;
    const list = Array.isArray(value) ? value : [value];
    for (const one of list) {
      if (one === undefined || one === null || one === '') continue;
      pairs.push(encodeURIComponent(key) + '=' + encodeURIComponent(one));
    }
  }
  return pairs.length ? '?' + pairs.join('&') : '';
}

// In-flight GETs by URL: `{controller, subscribers, promise}`. A panel that
// asks for a window another panel is already fetching joins that request
// instead of opening a second one, and the request is cancelled once the last
// subscriber has gone.
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
function read(url, signal) {
  let entry = inFlight.get(url);
  if (!entry) {
    const controller = new AbortController();
    entry = { controller, subscribers: 0, promise: null };
    entry.promise = request(url, { signal: controller.signal })
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
  session: (sessionId, signal) => read('/v1/sessions/' + encodeURIComponent(sessionId), signal),
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
    let response;
    try {
      response = await fetch('/v1/mcp', {
        method: 'POST',
        headers: authHeaders({
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
      if (unauthorizedHandler) unauthorizedHandler();
      throw new ApiError(401, 'The MCP endpoint needs a bearer token.',
        'Set a token from TRAZA_TOKENS with "Set token", then retry.');
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
