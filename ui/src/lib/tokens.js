import { fmtNum } from './format.js';

// A model call's tokens as the run tape shows them.
//
// With prompt caching on, providers disagree about what the input count
// counts (docs/llm-semantics.md, "Prompt caching, and what prompt tokens
// means"). Anthropic reports `input_tokens` as the UNCACHED remainder, with
// the cached part in the cache read and cache write counters, so a call that
// read 40k tokens from the cache reports an input of 2. OpenAI reports its
// prompt count INCLUSIVE of its cached subset, so adding the counters would
// count the cached part twice. The engine resolves this in
// `LlmFacts::context_tokens` (src/semconv.rs); this is the same rule, and a
// figure the server computed (`tokens.context`) is taken over it.

/** A count as the tape carries it, or null: a missing or malformed value is
    unreported, never NaN. */
function count(value) {
  return typeof value === 'number' && Number.isFinite(value) && value >= 0 ? value : null;
}

/** ASCII lowercase, as the engine's `to_ascii_lowercase`. */
function asciiLower(text) {
  return String(text || '').replace(/[A-Z]/g, (c) => c.toLowerCase());
}

/** How a provider counts cached prompt tokens against its input count:
    `exclusive` (input + cache read + cache write is the context),
    `inclusive` (the input count already is), or `unknown` — the engine's
    `LlmFacts::cache_convention`, provider first and the model only on Vertex,
    which serves both families under one name. */
export function cacheConvention(provider, model) {
  switch (provider) {
    case 'anthropic':
    case 'aws.bedrock':
      return 'exclusive';
    case 'gcp.vertex_ai': {
      const name = asciiLower(model);
      if (name.includes('claude')) return 'exclusive';
      if (name.includes('gemini')) return 'inclusive';
      return 'unknown';
    }
    case 'openai':
    case 'azure.ai.openai':
      return 'inclusive';
    default:
      return 'unknown';
  }
}

/** A step's tokens for the tape's "tokens in / out" cell: `{ text, input,
    title }`. `input` is the call's whole input — cache read and cache write
    included — where that can be said; `title` breaks it down, and is
    undefined for a step with no cache counters, which renders as it always
    did. Where the provider's convention is not known the reported input is
    shown unsummed, and the title says why. */
export function stepTokens(step) {
  const tokens = (step && step.tokens) || {};
  const input = count(tokens.input);
  const output = count(tokens.output);
  const read = count(tokens.cache_read);
  const write = count(tokens.cache_write);
  const cached = (read || 0) + (write || 0);
  const out = output == null ? '—' : fmtNum(output);
  const tail = ` · output ${output == null ? 'not reported' : out}`;
  if (input == null && output == null) return { text: '—', input: null, title: undefined };
  if (input == null || cached === 0) {
    return { text: `${input == null ? '—' : fmtNum(input)} / ${out}`, input, title: undefined };
  }
  const parts = [
    read ? `${fmtNum(read)} cache read` : null,
    write ? `${fmtNum(write)} cache write` : null,
  ].filter(Boolean).join(' + ');

  let convention;
  let why;
  if (Object.prototype.hasOwnProperty.call(tokens, 'context')) {
    const context = count(tokens.context);
    if (context == null) {
      convention = 'unknown';
      why = 'the server cannot tell whether this provider\'s input count includes them';
    } else if (context === input + cached) {
      convention = 'exclusive';
    } else if (context === input) {
      convention = 'inclusive';
    } else {
      return {
        text: `${fmtNum(context)} / ${out}`,
        input: context,
        title: `input ${fmtNum(context)}, as the server reads this call's context · reported ${fmtNum(input)} input, ${parts}${tail}`,
      };
    }
  } else if (typeof step.provider === 'string' && step.provider) {
    convention = cacheConvention(step.provider, step.model);
    why = `whether ${step.provider}'s input count includes them is not known`;
  } else {
    convention = 'unknown';
    why = 'the step names no provider, so whether its input count includes them cannot be told';
  }

  if (convention === 'exclusive') {
    const total = input + cached;
    return {
      text: `${fmtNum(total)} / ${out}`,
      input: total,
      title: `input ${fmtNum(total)} = ${fmtNum(input)} uncached + ${parts}${tail}`,
    };
  }
  if (convention === 'inclusive') {
    return { text: `${fmtNum(input)} / ${out}`, input, title: `input ${fmtNum(input)}, including ${parts}${tail}` };
  }
  return {
    text: `${fmtNum(input)} / ${out}`,
    input,
    title: `input ${fmtNum(input)} as reported · ${parts} reported beside it, not added: ${why}${tail}`,
  };
}
