// Who the dashboard is talking as, and whether it may talk at all.
//
// Two pieces of process-wide state live here rather than in React: the API
// client consults them on every request, and so does the poll hook, and both
// run outside any component. They are in a module of their own so that a
// test can mock the API client without also mocking them away.

/** The `GET /v1/whoami` answer for the current token, or null before it is
    known. */
let identity = null;

/** True from a 401 until a token is set. While it holds, nothing is sent:
    the poll hook skips its ticks and the client refuses locally. A rejected
    token re-sent every five seconds by four polls is the server's 401 log
    filling up for no reader, and a revoked token should go quiet, not keep
    knocking. */
let blocked = false;

export function getIdentity() { return identity; }

export function setIdentity(next) {
  identity = next || null;
}

export function isAuthBlocked() { return blocked; }

export function blockAuth() { blocked = true; }

export function unblockAuth() { blocked = false; }

// The store-global routes a tenant-bound credential is refused (`403
// required: unbound`). The list mirrors the server's, in
// docs/guide/http-api.md#conventions. Exact paths are exact: `/v1/stats` is
// an operator surface, `/v1/stats/llm` is ordinary tenant-scoped analytics.
const OPERATOR_EXACT = new Set([
  '/v1/stats',
  '/v1/metrics',
  '/v1/metrics.json',
  '/v1/verify',
  '/v1/flush',
  '/v1/checkpoint',
  '/v1/ledger',
  '/v1/ledger/verify',
  '/v1/ledger/anchors',
]);
const OPERATOR_PREFIXES = ['/v1/backups/', '/v1/admin/'];

/** Whether `path` (query string allowed) is an operator surface. */
export function isOperatorPath(path) {
  const bare = String(path || '').split('?')[0].replace(/\/+$/, '');
  if (OPERATOR_EXACT.has(bare)) return true;
  return OPERATOR_PREFIXES.some((prefix) => (bare + '/').startsWith(prefix));
}
