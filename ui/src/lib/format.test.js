import { describe, it, expect } from 'vitest';
import { durabilityMeans, fmtWindowLabel, fmtDelta, fmtUptime, fmtCostProvenance } from './format.js';

describe('durabilityMeans', () => {
  // Both screens used to print the `wal` sentence unconditionally, so a
  // `buffered` server — which guarantees the opposite — was told its writes
  // survived a kill-9. The wording lives in one place so no screen can
  // phrase a guarantee the server did not make.
  it('does not promise durability for a buffered server', () => {
    const said = durabilityMeans('buffered');
    expect(said).toMatch(/memory only|not durable/);
    expect(said).not.toMatch(/survives/);
  });

  it('states the wal guarantee only for wal', () => {
    expect(durabilityMeans('wal')).toMatch(/survives/);
  });

  it('describes flushed as sealed before the response', () => {
    expect(durabilityMeans('flushed')).toMatch(/sealed/);
  });

  it('says it does not know rather than guessing', () => {
    for (const unknown of [undefined, null, '', 'nonsense']) {
      const said = durabilityMeans(unknown);
      expect(said).toMatch(/not reported/);
      expect(said).not.toMatch(/survives/);
    }
  });
});

describe('fmtWindowLabel', () => {
  it('disambiguates a window whose ends share a clock time', () => {
    // A 24h window read "21:21Z – 21:21Z", which looks like an empty range.
    const since = Date.UTC(2026, 6, 25, 21, 21) * 1e6;
    const until = Date.UTC(2026, 6, 26, 21, 21) * 1e6;
    const label = fmtWindowLabel(since, until);
    expect(label).toContain('2026-07-25');
    expect(label).toContain('2026-07-26');
  });

  it('keeps a short window to clock time', () => {
    const since = Date.UTC(2026, 6, 26, 8, 0) * 1e6;
    const until = Date.UTC(2026, 6, 26, 9, 0) * 1e6;
    expect(fmtWindowLabel(since, until)).toBe('08:00 – 09:00Z');
  });

  it('says all time when unbounded', () => {
    expect(fmtWindowLabel(null, null)).toBe('all time');
  });
});

describe('fmtDelta', () => {
  it('has no opinion without a baseline', () => {
    expect(fmtDelta(10, 0)).toBe('');
    expect(fmtDelta(10, undefined)).toBe('');
  });

  it('signs the direction', () => {
    expect(fmtDelta(150, 100)).toBe('+50%');
    expect(fmtDelta(50, 100)).toBe('\u2212 50%'.replace(' ', ''));
  });
});

describe('fmtUptime', () => {
  it('reads as days and clock time', () => {
    expect(fmtUptime(((6 * 86400 + 4 * 3600 + 11 * 60) * 1e9))).toBe('6 d 04:11');
  });

  it('drops the day when there is none', () => {
    expect(fmtUptime(((4 * 3600 + 11 * 60) * 1e9))).toBe('04:11');
  });
});

describe('fmtCostProvenance', () => {
  // A cost that was measured and a cost that was worked out from list price
  // are different claims, and the screens show them in the same column.
  // Provenance comes from the CALL COUNTS: the dollars cannot carry it.

  const row = (over) => ({
    cost_usd: 0, cost_derived_usd: 0,
    cost_metered_calls: 0, cost_derived_calls: 0, cost_unpriced_calls: 0,
    ...over,
  });

  it('states a fully metered cost plainly', () => {
    const cost = fmtCostProvenance(row({ cost_usd: 0.42, cost_metered_calls: 1 }));
    expect(cost.text).toBe('0.4200');
    expect(cost.estimated).toBe(false);
    expect(cost.incomplete).toBe(false);
    expect(cost.title).toContain('Metered');
  });

  it('marks a fully derived cost as an estimate', () => {
    const cost = fmtCostProvenance(row({
      cost_usd: 11, cost_derived_usd: 11, cost_derived_calls: 2,
    }));
    expect(cost.text).toBe('~11.0000');
    expect(cost.estimated).toBe(true);
    expect(cost.title).toContain('configured model rates');
  });

  it('marks a mixed total as an estimate and reports the split', () => {
    const cost = fmtCostProvenance(row({
      cost_usd: 11.42, cost_derived_usd: 11,
      cost_metered_calls: 1, cost_derived_calls: 1,
    }));
    expect(cost.text).toBe('~11.4200');
    expect(cost.estimated).toBe(true);
    expect(cost.title).toContain('1 metered');
    expect(cost.title).toContain('11.0000');
  });

  it('does not call an unpriced total metered', () => {
    // No pricing configured and no metered cost: every call contributed
    // nothing. Reading `cost_derived_usd === 0` as proof of measurement said
    // "metered by the spans themselves" about spans that reported no cost
    // at all.
    const cost = fmtCostProvenance(row({ cost_usd: 0, cost_unpriced_calls: 3 }));
    expect(cost.estimated).toBe(false);
    expect(cost.incomplete).toBe(true);
    expect(cost.text).toBe('—');
    expect(cost.title).not.toContain('Metered by the spans');
    expect(cost.title).toContain('undercount');
    expect(cost.title).toContain('3 with no cost');
  });

  it('marks a zero-rate model as an estimate despite costing nothing', () => {
    // A rate of 0.0 is legal and explicitly supported — it is how a
    // self-hosted model is priced. It derives a cost of exactly $0.00, which
    // on the dollars alone is indistinguishable from never having been
    // priced, and from having been metered at zero.
    const cost = fmtCostProvenance(row({
      cost_usd: 0, cost_derived_usd: 0, cost_derived_calls: 4,
    }));
    expect(cost.estimated).toBe(true);
    expect(cost.text).toBe('~0.0000');
    expect(cost.title).toContain('4 priced from the configured model rates');
  });

  it('reports an estimate that is also an undercount as both', () => {
    const cost = fmtCostProvenance(row({
      cost_usd: 5, cost_derived_usd: 5, cost_derived_calls: 1, cost_unpriced_calls: 2,
    }));
    expect(cost.estimated).toBe(true);
    expect(cost.incomplete).toBe(true);
    expect(cost.title).toContain('Estimated and an undercount');
  });

  it('says so when there were no LLM calls at all', () => {
    expect(fmtCostProvenance(row({})).title).toBe('No LLM calls here.');
    expect(fmtCostProvenance(row({})).text).toBe('—');
  });

  it('still shows a metered zero as a zero', () => {
    // A free tier really did charge nothing, and the span measured that.
    const cost = fmtCostProvenance(row({ cost_usd: 0, cost_metered_calls: 2 }));
    expect(cost.text).toBe('0.0000');
    expect(cost.estimated).toBe(false);
  });

  it('survives a row from a server that predates the counts', () => {
    expect(fmtCostProvenance({ cost_usd: 0.42 }).estimated).toBe(false);
    expect(fmtCostProvenance(undefined).text).toBe('—');
  });
});
