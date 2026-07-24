import { describe, it, expect } from 'vitest';
import { tokenize, normalizeLanguage } from './highlight.js';

const joined = (code, language) => tokenize(code, language).map((token) => token.text).join('');
const typed = (code, language, type) =>
  tokenize(code, language).filter((token) => token.type === type).map((token) => token.text);

describe('tokenize', () => {
  // The property that matters most: highlighting must never alter the code it
  // displays. Every token stream has to concatenate back to the exact source.
  it.each([
    ['json', '{"a": 1, "b": [true, null], "c": "x"}'],
    ['sql', "select count(*) from t -- note\nwhere a >= now()"],
    ['python', 'def f(x):\n    # c\n    return "hi" if x else None'],
    ['javascript', 'const a = 42; /* b */ let s = `t`;'],
    ['rust', 'pub fn main() { let x: u8 = 0xFF; }'],
    ['shell', 'cd /tmp && echo "hi" # done'],
    ['yaml', 'key: value\nother: 12 # c'],
    [undefined, 'anything(1) "two" // three'],
    ['totally-unknown', 'foo(1) /* c */'],
  ])('round-trips %s exactly', (language, code) => {
    expect(joined(code, language)).toBe(code);
  });

  it('distinguishes JSON keys from string values', () => {
    const code = '{"key": "value"}';
    expect(typed(code, 'json', 'property')).toEqual(['"key"']);
    expect(typed(code, 'json', 'string')).toEqual(['"value"']);
  });

  it('finds keywords, comments and numbers in SQL', () => {
    const code = "select 1 from t -- trailing";
    expect(typed(code, 'sql', 'keyword')).toEqual(['select', 'from']);
    expect(typed(code, 'sql', 'number')).toEqual(['1']);
    expect(typed(code, 'sql', 'comment')).toEqual(['-- trailing']);
  });

  it('is case-insensitive only where the language is', () => {
    expect(typed('SELECT 1', 'sql', 'keyword')).toEqual(['SELECT']);
    expect(typed('CONST x', 'javascript', 'keyword')).toEqual([]);
  });

  it('still finds strings and comments in an unknown language', () => {
    expect(typed('x "s" // c', 'totally-unknown', 'string')).toEqual(['"s"']);
    expect(typed('x "s" // c', 'totally-unknown', 'comment')).toEqual(['// c']);
  });

  it('does not run away on an unterminated string', () => {
    const code = 'a = "never closed\nb = 1';
    expect(joined(code, 'python')).toBe(code);
  });

  it('leaves very large input untouched rather than stalling', () => {
    const code = 'x'.repeat(200000);
    const tokens = tokenize(code, 'javascript');
    expect(tokens).toHaveLength(1);
    expect(tokens[0].type).toBe('plain');
  });

  it('handles empty and nullish input', () => {
    expect(tokenize('', 'json')).toEqual([]);
    expect(tokenize(null, 'json')).toEqual([]);
  });
});

describe('normalizeLanguage', () => {
  it('maps aliases to a canonical id', () => {
    expect(normalizeLanguage('ts')).toBe('javascript');
    expect(normalizeLanguage('py')).toBe('python');
    expect(normalizeLanguage('bash')).toBe('shell');
    expect(normalizeLanguage('JSON')).toBe('json');
    expect(normalizeLanguage('nonsense')).toBeNull();
    expect(normalizeLanguage(undefined)).toBeNull();
  });
});
