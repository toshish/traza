import { describe, it, expect } from 'vitest';
import {
  predicate, toParams, toHash, fromHash, toCurl, opsFor, windowOf, emptyQuery,
} from './query.js';

/** A query built from predicates, as the Traces screen builds one. */
function withPreds(...preds) {
  return { ...emptyQuery(), range: 'all', preds };
}

describe('predicates map onto the API the store actually exposes', () => {
  it('sends span columns as their own parameters', () => {
    const params = toParams(withPreds(
      predicate('service', '=', 'checkout'),
      predicate('name', '=', 'charge'),
    ));
    expect(params.service).toBe('checkout');
    expect(params.name).toBe('charge');
  });

  it('routes each operator to its own attribute family', () => {
    const params = toParams(withPreds(
      predicate('attr.region', '=', 'us-east'),
      predicate('attr.tier', '≠', 'free'),
      predicate('attr.llm.usage.total_tokens', '≥', '4000'),
      predicate('attr.llm.cost_usd', '≤', '0.01'),
    ));
    expect(params['attr.region']).toBe('us-east');
    expect(params['not_attr.tier']).toBe('free');
    expect(params['min_attr.llm.usage.total_tokens']).toBe('4000');
    expect(params['max_attr.llm.cost_usd']).toBe('0.01');
  });

  it('repeats a key when two predicates share it', () => {
    // The old form had exactly one attribute pair, so two conditions on one
    // key were impossible to express at all.
    const params = toParams(withPreds(
      predicate('attr.llm.usage.total_tokens', '≥', '1000'),
      predicate('attr.llm.usage.total_tokens', '≤', '9000'),
      predicate('attr.tag', '=', 'a'),
      predicate('attr.tag', '=', 'b'),
    ));
    expect(params['attr.tag']).toEqual(['a', 'b']);
    expect(params['min_attr.llm.usage.total_tokens']).toBe('1000');
    expect(params['max_attr.llm.usage.total_tokens']).toBe('9000');
  });

  it('maps status onto the span field, not an attribute', () => {
    // attr.status reads an attribute most instrumentation never writes;
    // the span's own status is a different thing and needs its own parameter.
    expect(toParams(withPreds(predicate('status', '=', 'error'))).status).toBe('error');
    expect(toParams(withPreds(predicate('status', '≠', 'error'))).not_status).toBe('error');
    expect(toParams(withPreds(predicate('status', '=', 'error')))['attr.status']).toBeUndefined();
  });

  it('bounds duration from both ends', () => {
    const params = toParams(withPreds(
      predicate('duration_ms', '≥', '100'),
      predicate('duration_ms', '≤', '2000'),
    ));
    expect(params.min_duration_ms).toBe('100');
    expect(params.max_duration_ms).toBe('2000');
  });

  it('drops predicates with no value rather than sending empty filters', () => {
    const params = toParams(withPreds(predicate('service', '=', ''), predicate('name', '=', 'x')));
    expect(params.service).toBeUndefined();
    expect(params.name).toBe('x');
  });
});

describe('content search', () => {
  it('maps onto the API parameter the store understands', () => {
    const params = toParams({ ...withPreds(), content: 'refund order' });
    expect(params.content).toBe('refund order');
  });

  it('composes with predicates rather than replacing them', () => {
    const params = toParams({
      ...withPreds(predicate('service', '=', 'checkout')),
      content: 'refund',
    });
    expect(params.content).toBe('refund');
    expect(params.service).toBe('checkout');
  });

  it('is dropped when blank, so an empty box is not a filter', () => {
    expect(toParams({ ...withPreds(), content: '   ' }).content).toBeUndefined();
  });

  it('round-trips through the URL alongside predicates', () => {
    // `c`, not `q` — the hash spends `q` on the predicate list.
    const original = { ...withPreds(predicate('status', '=', 'error')), content: 'refund order' };
    const hash = toHash(original);
    expect(hash.c).toBe('refund order');
    const restored = fromHash(new URLSearchParams(hash));
    expect(restored.content).toBe('refund order');
    expect(toParams(restored)).toEqual(toParams(original));
  });

  it('reaches the curl command', () => {
    const command = toCurl({ ...withPreds(), content: 'refund order' }, 'http://x');
    expect(command).toContain('content=refund%20order');
  });
});

describe('a query survives a round trip through the URL', () => {
  it('restores every predicate', () => {
    const original = withPreds(
      predicate('service', '=', 'checkout'),
      predicate('attr.tier', '≠', 'free'),
      predicate('duration_ms', '≥', '250'),
    );
    const restored = fromHash(new URLSearchParams(toHash(original)));
    expect(toParams(restored)).toEqual(toParams(original));
  });

  it('carries values containing the separators intact', () => {
    // A value with ~ or | would split the encoding in half if it were not
    // escaped, silently turning one predicate into two wrong ones.
    const original = withPreds(predicate('attr.path', '=', 'a~b|c%d'));
    const restored = fromHash(new URLSearchParams(toHash(original)));
    expect(restored.preds[0].value).toBe('a~b|c%d');
  });

  it('keeps sort and limit', () => {
    const original = { ...withPreds(predicate('service', '=', 'x')), sort: '-duration', limit: 500 };
    const restored = fromHash(new URLSearchParams(toHash(original)));
    expect(restored.sort).toBe('-duration');
    expect(restored.limit).toBe(500);
  });

  it('reads an empty hash as an empty query', () => {
    const restored = fromHash(new URLSearchParams(''));
    expect(restored.preds).toEqual([]);
  });
});

describe('operators are offered only where they mean something', () => {
  it('gives duration only its bounds', () => {
    expect(opsFor('duration_ms')).toEqual(['≥', '≤']);
  });

  it('gives an exact-match column only equality', () => {
    expect(opsFor('service')).toEqual(['=']);
  });

  it('gives an attribute the full set', () => {
    expect(opsFor('attr.anything')).toEqual(['=', '≠', '≥', '≤']);
  });
});

describe('time windows', () => {
  it('resolves a relative range against now, not against when it was saved', () => {
    // A shared "last hour" link must mean the recipient's last hour.
    const now = 1_700_000_000_000;
    const { sinceNs, untilNs } = windowOf('1h', now);
    expect(untilNs).toBe(now * 1e6);
    expect(untilNs - sinceNs).toBe(3600e3 * 1e6);
  });

  it('leaves "all" unbounded so the server prunes nothing', () => {
    expect(windowOf('all')).toEqual({ sinceNs: null, untilNs: null });
  });

  it('passes an absolute window through untouched', () => {
    const absolute = { sinceNs: 111, untilNs: 222 };
    expect(windowOf(absolute)).toEqual(absolute);
  });
});

describe('curl reproduces what is on screen', () => {
  it('includes every predicate and repeats shared keys', () => {
    const command = toCurl(withPreds(
      predicate('service', '=', 'checkout'),
      predicate('attr.tag', '=', 'a'),
      predicate('attr.tag', '=', 'b'),
    ), 'http://localhost:8080');
    expect(command).toContain('service=checkout');
    expect(command).toContain('attr.tag=a');
    expect(command).toContain('attr.tag=b');
    expect(command).toContain('/v1/spans?');
  });

  it('percent-encodes values so a shell cannot reinterpret them', () => {
    const command = toCurl(withPreds(predicate('attr.q', '=', 'a b&c')), 'http://x');
    expect(command).toContain('attr.q=a%20b%26c');
  });
});
