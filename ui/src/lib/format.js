// Formatting rules from the design system: numbers are mono + tabular,
// durations pick a stable unit, timestamps are UTC, costs are USD with the
// API's own precision (4 decimals).

export function fmtDurationNs(ns) {
  const ms = ns / 1e6;
  if (ms >= 1000) return (ms / 1000).toFixed(2) + ' s';
  if (ms >= 1) return ms.toFixed(2) + ' ms';
  return (ms * 1000).toFixed(0) + ' µs';
}

export function fmtNum(value) {
  return typeof value === 'number' ? value.toLocaleString('en-US') : String(value ?? '');
}

/** Headline figures only: a billion-token total does not fit a stat tile at
    24px mono, and a clipped number is worse than a rounded one. Tables and
    tooltips keep the exact value via fmtNum. */
export function fmtCompact(value) {
  if (typeof value !== 'number' || !Number.isFinite(value)) return String(value ?? '');
  const abs = Math.abs(value);
  if (abs >= 1e12) return (value / 1e12).toFixed(2) + ' T';
  if (abs >= 1e9) return (value / 1e9).toFixed(2) + ' B';
  if (abs >= 1e6) return (value / 1e6).toFixed(2) + ' M';
  return value.toLocaleString('en-US');
}

export function fmtCost(value) {
  return typeof value === 'number' ? value.toFixed(4) : String(value ?? '');
}

export function fmtBytes(bytes) {
  if (typeof bytes !== 'number') return String(bytes ?? '');
  if (bytes >= 1 << 30) return (bytes / (1 << 30)).toFixed(2) + ' GiB';
  if (bytes >= 1 << 20) return (bytes / (1 << 20)).toFixed(2) + ' MiB';
  if (bytes >= 1 << 10) return (bytes / (1 << 10)).toFixed(1) + ' KiB';
  return bytes + ' B';
}

/** Nanosecond Unix timestamp → "2026-07-23 14:03:21Z" (UTC, mono-friendly). */
export function fmtTimeNs(ns) {
  if (!ns) return '';
  const date = new Date(ns / 1e6);
  return date.toISOString().replace('T', ' ').replace(/\.\d+Z$/, 'Z');
}

/** Compact activity window: "14:03:21–14:09:40Z" or across days, both stamps. */
export function fmtWindow(firstNs, lastNs) {
  if (!firstNs || !lastNs) return '';
  const first = new Date(firstNs / 1e6).toISOString();
  const last = new Date(lastNs / 1e6).toISOString();
  if (first.slice(0, 10) === last.slice(0, 10)) {
    return first.slice(0, 10) + ' ' + first.slice(11, 19) + '–' + last.slice(11, 19) + 'Z';
  }
  return first.slice(0, 16).replace('T', ' ') + '–' + last.slice(0, 16).replace('T', ' ') + 'Z';
}

/** Average LLM latency for an aggregate row, or "" when no calls. */
export function fmtAvgLatency(durationNs, calls) {
  if (!calls) return '';
  return fmtDurationNs(durationNs / calls);
}
