// @vitest-environment jsdom
//
// The URL-fragment token handoff: which fragments carry a token, what is left
// in the address bar afterwards, and that the token lands where the "Set
// token" prompt puts it.

import { describe, it, expect, vi, beforeEach } from 'vitest';
import { consumeFragmentToken, takeTokenFromHash } from './handoff.js';
import { getToken, setToken } from './api.js';

beforeEach(() => {
  setToken('');
  sessionStorage.clear();
});

describe('takeTokenFromHash', () => {
  it('takes a whole-fragment token and leaves no fragment', () => {
    expect(takeTokenFromHash('#token=trz_abc')).toEqual({ token: 'trz_abc', hash: '' });
  });

  it('percent-decodes the token', () => {
    expect(takeTokenFromHash('#token=a%2Bb%3Dc')).toEqual({ token: 'a+b=c', hash: '' });
  });

  it('takes a token from a route query and keeps the route and its other parameters', () => {
    expect(takeTokenFromHash('#/run/sess-1?tenant=acme&token=trz_x&step=2'))
      .toEqual({ token: 'trz_x', hash: '#/run/sess-1?tenant=acme&step=2' });
    expect(takeTokenFromHash('#/overview?token=trz_y')).toEqual({ token: 'trz_y', hash: '#/overview' });
  });

  it('leaves every other fragment alone', () => {
    for (const hash of ['', '#', '#/traces', '#/traces?q=a', '#tokens=x', '#/token=x']) {
      expect(takeTokenFromHash(hash)).toEqual({ token: null, hash });
    }
  });
});

describe('consumeFragmentToken', () => {
  const fakeWindow = (hash) => ({
    location: { hash, pathname: '/x/org/project/', search: '?from=cloud' },
    history: { state: { kept: true }, replaceState: vi.fn() },
  });

  it('stores the token as the prompt does and strips it with replaceState', () => {
    const win = fakeWindow('#/guard?token=trz_handoff&state=stop');
    expect(consumeFragmentToken(win)).toBe(true);
    expect(getToken()).toBe('trz_handoff');
    expect(sessionStorage.getItem('traza_token')).toBe('trz_handoff');
    expect(win.history.replaceState).toHaveBeenCalledWith({ kept: true }, '', '/x/org/project/?from=cloud#/guard?state=stop');
  });

  it('does nothing, and keeps any existing token, when the fragment carries none', () => {
    setToken('existing');
    const win = fakeWindow('#/traces');
    expect(consumeFragmentToken(win)).toBe(false);
    expect(getToken()).toBe('existing');
    expect(win.history.replaceState).not.toHaveBeenCalled();
  });

  it('ignores a token in the query string, which servers and proxies log', () => {
    const win = { location: { hash: '', pathname: '/', search: '?token=leaked' }, history: { replaceState: vi.fn() } };
    expect(consumeFragmentToken(win)).toBe(false);
    expect(getToken()).toBe('');
  });
});
