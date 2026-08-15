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

  it('states a fully metered cost plainly', () => {
    const cost = fmtCostProvenance(0.42, 0);
    expect(cost.text).toBe('0.4200');
    expect(cost.estimated).toBe(false);
  });

  it('marks a fully derived cost as an estimate', () => {
    const cost = fmtCostProvenance(11, 11);
    expect(cost.text).toBe('~11.0000');
    expect(cost.estimated).toBe(true);
    expect(cost.title).toContain('model pricing');
  });

  it('marks a mixed total as an estimate and reports the split', () => {
    // The whole figure is an estimate once any of it is, and the title has to
    // say how much — otherwise "~" is a warning with no way to act on it.
    const cost = fmtCostProvenance(11.42, 11);
    expect(cost.text).toBe('~11.4200');
    expect(cost.estimated).toBe(true);
    expect(cost.title).toContain('0.4200 metered');
    expect(cost.title).toContain('11.0000 derived');
  });

  it('treats a missing derived share as metered', () => {
    // Older servers, and every response before pricing existed, omit it.
    expect(fmtCostProvenance(0.42, undefined).estimated).toBe(false);
  });
});
