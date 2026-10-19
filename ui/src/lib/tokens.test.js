import { describe, it, expect } from 'vitest';
import { cacheConvention, stepTokens } from './tokens.js';

// A step's tokens as the run tape shows them. With prompt caching on,
// providers disagree about what the input count counts (docs/llm-semantics.md,
// "Prompt caching, and what prompt tokens means"): Anthropic's excludes the
// cached part, OpenAI's includes it. Synthetic counts throughout.

const step = (tokens, extra = {}) => ({ kind: 'llm', model: 'm', tokens, ...extra });
const ANTHROPIC = { input: 2, output: 555, cache_read: 40011, cache_write: 200 };

describe('cacheConvention', () => {
  it('follows the engine table: provider first, the model only on Vertex', () => {
    expect(cacheConvention('anthropic')).toBe('exclusive');
    expect(cacheConvention('aws.bedrock', 'anything')).toBe('exclusive');
    expect(cacheConvention('openai')).toBe('inclusive');
    expect(cacheConvention('azure.ai.openai')).toBe('inclusive');
    expect(cacheConvention('gcp.vertex_ai', 'Claude-Sonnet-5-5@2026')).toBe('exclusive');
    expect(cacheConvention('gcp.vertex_ai', 'gemini-3.8-flash')).toBe('inclusive');
    expect(cacheConvention('gcp.vertex_ai', 'llama-3')).toBe('unknown');
    expect(cacheConvention('gcp.vertex_ai')).toBe('unknown');
    // OpenInference host names, and a provider not in the table.
    expect(cacheConvention('aws')).toBe('unknown');
    expect(cacheConvention('openrouter', 'claude-sonnet-5-5')).toBe('unknown');
    expect(cacheConvention(undefined, 'claude-sonnet-5-5')).toBe('unknown');
  });
});

describe('stepTokens', () => {
  it("adds Anthropic's cache read and write to its uncached input", () => {
    const out = stepTokens(step(ANTHROPIC, { provider: 'anthropic' }));
    expect(out.text).toBe('40,213 / 555');
    expect(out.input).toBe(40213);
    expect(out.title).toBe('input 40,213 = 2 uncached + 40,011 cache read + 200 cache write · output 555');
  });

  it("prefers the server's context figure, and breaks it down the same way", () => {
    const out = stepTokens(step({ ...ANTHROPIC, context: 40213 }));
    expect(out.text).toBe('40,213 / 555');
    expect(out.title).toBe('input 40,213 = 2 uncached + 40,011 cache read + 200 cache write · output 555');
  });

  it('leaves out a cache counter that read nothing', () => {
    const out = stepTokens(step({ input: 5, output: 9, cache_read: 1000, cache_write: 0 }, { provider: 'anthropic' }));
    expect(out.text).toBe('1,005 / 9');
    expect(out.title).toBe('input 1,005 = 5 uncached + 1,000 cache read · output 9');
  });

  it("does not add OpenAI's cached tokens, which its prompt count already includes", () => {
    const out = stepTokens(step({ input: 1200, output: 40, cache_read: 1024, cache_write: null }, { provider: 'openai' }));
    expect(out.text).toBe('1,200 / 40');
    expect(out.input).toBe(1200);
    expect(out.title).toBe('input 1,200, including 1,024 cache read · output 40');
    // The server's figure says the same: the context is the prompt count.
    expect(stepTokens(step({ input: 1200, output: 40, cache_read: 1024, cache_write: null, context: 1200 })).title)
      .toBe('input 1,200, including 1,024 cache read · output 40');
  });

  it('sums nothing where the convention is not known, and says why', () => {
    const unnamed = stepTokens(step(ANTHROPIC));
    expect(unnamed.text).toBe('2 / 555');
    expect(unnamed.title).toMatch(/^input 2 as reported · 40,011 cache read \+ 200 cache write reported beside it, not added: /);
    expect(unnamed.title).toMatch(/names no provider/);
    const other = stepTokens(step(ANTHROPIC, { provider: 'openrouter' }));
    expect(other.text).toBe('2 / 555');
    expect(other.title).toMatch(/openrouter/);
    // The server read it and could not tell: `context` is null.
    const ambiguous = stepTokens(step({ ...ANTHROPIC, context: null }, { provider: 'anthropic' }));
    expect(ambiguous.text).toBe('2 / 555');
    expect(ambiguous.title).toMatch(/not added/);
  });

  it('renders a step with no cache counters as before', () => {
    expect(stepTokens(step({ input: 82, output: 17, cache_read: null, cache_write: null }, { provider: 'anthropic' })))
      .toEqual({ text: '82 / 17', input: 82, title: undefined });
    expect(stepTokens(step({ input: 82, output: 17, cache_read: 0, cache_write: 0 }, { provider: 'anthropic' })))
      .toEqual({ text: '82 / 17', input: 82, title: undefined });
    expect(stepTokens(step({ input: 82, output: 17 }))).toEqual({ text: '82 / 17', input: 82, title: undefined });
  });

  it('never shows NaN for missing or malformed counts', () => {
    expect(stepTokens({ kind: 'tool', tokens: { input: null, output: null, cache_read: null, cache_write: null } }).text).toBe('—');
    expect(stepTokens({ kind: 'tool' }).text).toBe('—');
    expect(stepTokens(null).text).toBe('—');
    const noInput = stepTokens(step({ input: null, output: 5, cache_read: 300, cache_write: null }, { provider: 'anthropic' }));
    expect(noInput.text).toBe('— / 5');
    const noOutput = stepTokens(step({ input: 2, output: null, cache_read: 300, cache_write: undefined }, { provider: 'anthropic' }));
    expect(noOutput.text).toBe('302 / —');
    expect(noOutput.title).toBe('input 302 = 2 uncached + 300 cache read · output not reported');
    const junk = stepTokens(step({ input: 2, output: 'x', cache_read: Number.NaN, cache_write: '7' }, { provider: 'anthropic' }));
    expect(junk.text).toBe('2 / —');
    for (const value of [noInput, noOutput, junk, stepTokens(step({ ...ANTHROPIC, context: 'n' }))]) {
      expect(`${value.text} ${value.title || ''}`).not.toMatch(/NaN|undefined/);
    }
  });
});
