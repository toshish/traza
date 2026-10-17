// Token handoff through the URL fragment.
//
// A control plane that mints a credential for somebody can send them straight
// to the dashboard with it: `https://host/#token=<t>`. The fragment is the
// one part of a URL a browser never sends to a server, so the token reaches
// this page and no access log. It is read once at startup, stored exactly as
// the "Set token" prompt stores it (sessionStorage), and removed from the
// address bar with `history.replaceState`, so it is not left in the current
// history entry, a bookmark, or a screenshot of the address bar.
//
// The rule, exactly:
//
//   #token=<t>                  the whole fragment is the token; the page opens
//                               at the default screen
//   #/<route>?…&token=<t>&…     a route whose query carries `token`; the page
//                               opens at that route with `token` removed and
//                               every other parameter kept
//
// `<t>` is percent-decoded. Nothing else is recognized: a `token` anywhere
// else in the URL (the query string before `#`, which IS sent to servers) is
// ignored rather than honoured, so a link cannot teach anyone to put a
// credential where it gets logged.

import { setToken } from './api.js';

/** Splits a token out of a fragment. Returns `{token, hash}`: the token (or
    null when the fragment carries none) and the fragment to leave behind. */
export function takeTokenFromHash(hash) {
  const raw = String(hash || '');
  if (raw.startsWith('#token=')) {
    const token = safeDecode(raw.slice('#token='.length));
    return { token: token || null, hash: '' };
  }
  if (!raw.startsWith('#/')) return { token: null, hash: raw };
  const at = raw.indexOf('?');
  if (at < 0) return { token: null, hash: raw };
  const params = new URLSearchParams(raw.slice(at + 1));
  if (!params.has('token')) return { token: null, hash: raw };
  const token = params.get('token');
  params.delete('token');
  const rest = params.toString();
  return { token: token || null, hash: raw.slice(0, at) + (rest ? '?' + rest : '') };
}

function safeDecode(text) {
  try { return decodeURIComponent(text); } catch (e) { return text; }
}

/** Reads a handed-off token from the current URL once, stores it, and strips
    it from the address bar. Returns true when a token was taken. */
export function consumeFragmentToken(win = typeof window !== 'undefined' ? window : undefined) {
  if (!win || !win.location) return false;
  const { token, hash } = takeTokenFromHash(win.location.hash);
  if (token == null) return false;
  setToken(token);
  const url = win.location.pathname + win.location.search + hash;
  try {
    win.history.replaceState(win.history.state, '', url);
  } catch (e) {
    // A sandboxed frame can refuse replaceState. Falling back to assigning the
    // hash still removes the token from what is displayed, at the cost of a
    // history entry that holds it.
    win.location.hash = hash;
  }
  return true;
}
