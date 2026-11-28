// Where the API lives, relative to this page.
//
// The dashboard is usually served by traza-server itself, so every request is
// same-origin and the base is empty: `/v1/spans` means this host's `/v1/spans`.
// Behind a reverse proxy that mounts a store under a path prefix
// (`https://host/x/org/project/`), the page is served from that prefix and the
// API with it, and an absolute `/v1/...` would reach the proxy's own root
// instead. The deployer says so once, in the page:
//
//   <meta name="traza-api-base" content="/x/org/project">
//
// and every request, stream, MCP call and copyable snippet is built from it.
// It is read from the document rather than from the URL on purpose: a page
// must not let a link choose where its bearer token is sent.

/** The configured API base, without a trailing slash; empty by default.

    Read on every call rather than cached at import: it costs one selector
    lookup, and a cached value would make the tag impossible to change in a
    test (or by a host page that injects it after load). */
export function apiBase() {
  if (typeof document === 'undefined') return '';
  const meta = document.querySelector('meta[name="traza-api-base"]');
  const raw = (meta && meta.getAttribute('content')) || '';
  return raw.trim().replace(/\/+$/, '');
}

/** The URL a request for `path` (which starts with `/v1`) is sent to. */
export function apiUrl(path) {
  return apiBase() + path;
}

/** The absolute base a copyable snippet should name — a curl command, an
    OTLP endpoint, an SDK base URL. It is the page's origin plus the base,
    unless the base is itself absolute. Read from the page so a snippet is
    correct behind any host, port or proxy without anyone typing it. */
export function publicBase() {
  const base = apiBase();
  if (/^[a-z][a-z0-9+.-]*:\/\//i.test(base)) return base;
  const origin = typeof window !== 'undefined' && window.location ? window.location.origin : 'http://localhost:8080';
  return origin + base;
}
