// Who the dashboard is talking as, and whether it may talk at all.
//
// Two pieces of process-wide state live here rather than in React: the API
// client consults them on every request, and so does the poll hook, and both
// run outside any component. They are in a module of their own so that a
// test can mock the API client without also mocking them away.

/** Which credential the tab holds, as a count of changes to it.

    An identity is an answer about one token. Kept without saying which, it
    outlived the token it described: after a change the old "operator" stood
    until React got round to clearing it, and screens mounted for the new
    token asked operator surfaces with it in between. Every answer is now
    recorded against the generation it was asked under, and a change forgets
    the old one in the same step that makes it. A count rather than the token
    itself, because the same token set again later is a new question too: the
    server may have revoked or rebound it since. */
let generation = 0;

/** The `GET /v1/whoami` answer for the current generation, or null before it
    is known. */
let identity = null;

/** True from a 401 until a token is set. While it holds, nothing is sent:
    the poll hook skips its ticks and the client refuses locally. A rejected
    token re-sent every five seconds by four polls is the server's 401 log
    filling up for no reader, and a revoked token should go quiet, not keep
    knocking. */
let blocked = false;

const listeners = new Set();

export function credentialGeneration() { return generation; }

/** Calls `listener` after every credential change; returns the unsubscribe.
    The shape `useSyncExternalStore` takes, so the app re-renders under the
    new generation in the same step rather than an effect later. */
export function onCredentialChange(listener) {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}

/** The tab now holds a different credential, or none. The identity goes in
    the same step, so nothing is sent on the strength of an answer about the
    old one, and the 401 block lifts: a new token, or deliberately none, is a
    new question for the server. */
export function credentialChanged() {
  generation += 1;
  identity = null;
  blocked = false;
  for (const listener of [...listeners]) listener();
}

export function getIdentity() { return identity; }

/** Records `next` as the identity of generation `asked` — the current one
    unless said otherwise. Returns false, recording nothing, when the
    credential has changed since the question was asked: that answer
    describes a token this tab no longer sends. */
export function setIdentity(next, asked = generation) {
  if (asked !== generation) return false;
  identity = next || null;
  return true;
}

export function isAuthBlocked() { return blocked; }

export function blockAuth() { blocked = true; }

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
