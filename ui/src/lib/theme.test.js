// @vitest-environment jsdom
//
// The theme before first paint. index.html runs a script before the app
// loads; it compared the stored value with 'dark', but the app stores JSON
// ('"dark"' with its quotes), so a dark reload always painted paper first.
// The script is read out of index.html and run here, against both spellings
// and against the OS preference.

import { describe, it, expect, beforeEach } from 'vitest';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { readStored } from './route.js';

// Vitest runs from ui/, where index.html lives (import.meta.url is not a
// file: URL under the jsdom environment).
const html = readFileSync(resolve(process.cwd(), 'index.html'), 'utf8');
const script = /<script>([\s\S]*?)<\/script>/.exec(html)[1];

function prepaint({ stored, osDark }) {
  document.documentElement.removeAttribute('data-theme');
  localStorage.clear();
  if (stored !== undefined) localStorage.setItem('traza_theme', stored);
  window.matchMedia = (query) => ({ matches: Boolean(osDark) && query.includes('dark'), media: query });
  // eslint-disable-next-line no-new-func
  new Function(script)();
  return document.documentElement.getAttribute('data-theme');
}

beforeEach(() => localStorage.clear());

describe('the pre-paint theme', () => {
  it('reads the JSON the app stores', () => {
    expect(prepaint({ stored: '"dark"', osDark: false })).toBe('dark');
    expect(prepaint({ stored: '"light"', osDark: true })).toBe(null);
  });

  it('still reads a bare value an older build stored', () => {
    expect(prepaint({ stored: 'dark', osDark: false })).toBe('dark');
  });

  it('follows the OS when nothing is stored', () => {
    expect(prepaint({ osDark: true })).toBe('dark');
    expect(prepaint({ osDark: false })).toBe(null);
  });
});

describe('readStored', () => {
  it('parses JSON, tolerates a bare string, and falls back when absent', () => {
    localStorage.setItem('k', '"dark"');
    expect(readStored('k', null)).toBe('dark');
    localStorage.setItem('k', 'dark');
    expect(readStored('k', null)).toBe('dark');
    localStorage.removeItem('k');
    expect(readStored('k', 'fallback')).toBe('fallback');
  });
});
