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

/** Clock time only: "08:20Z". Axis ticks and dense rows have no room for the
    date, and every one of them shares the window shown above. */
export function fmtClockNs(ns) {
  if (!ns) return '';
  return new Date(ns / 1e6).toISOString().slice(11, 16) + 'Z';
}

/** How long ago, coarsely: "4m", "2h", "3d". Recency is a glance, not a
    measurement — anything finer invites reading precision that is not there. */
export function fmtAgo(ns, now = Date.now()) {
  if (!ns) return '';
  const seconds = Math.max(0, (now - ns / 1e6) / 1000);
  if (seconds < 60) return Math.floor(seconds) + 's';
  if (seconds < 3600) return Math.floor(seconds / 60) + 'm';
  if (seconds < 86400) return Math.floor(seconds / 3600) + 'h';
  return Math.floor(seconds / 86400) + 'd';
}

/** A rate as a per-second figure: "4,812/s". */
export function fmtRate(perSecond) {
  if (!Number.isFinite(perSecond)) return '—';
  if (perSecond >= 1000) return Math.round(perSecond).toLocaleString('en-US') + '/s';
  if (perSecond >= 10) return perSecond.toFixed(0) + '/s';
  return perSecond.toFixed(1) + '/s';
}

/** A ratio as a whole percent: "96%". */
export function fmtPercent(part, whole) {
  if (!whole) return '0%';
  return Math.round((part / whole) * 100) + '%';
}

/** A signed relative change: "+41%", "−12%", or "—" when there is no base to
    compare against. The minus is a real minus sign, not a hyphen: these sit
    in tabular-nums columns where a hyphen reads a full step narrower. */
export function fmtDelta(current, previous) {
  if (!previous || !Number.isFinite(current) || !Number.isFinite(previous)) return '';
  const change = (current - previous) / previous;
  if (!Number.isFinite(change)) return '';
  const sign = change >= 0 ? '+' : '−';
  const magnitude = Math.abs(change);
  if (magnitude >= 10) return sign + magnitude.toFixed(1) + '×';
  return sign + Math.round(magnitude * 100) + '%';
}

/** A window's endpoints, disambiguated.

    Clock-only endpoints are ambiguous for exactly the windows people use
    most: a 24h range reads "21:21Z – 21:21Z", which looks like an empty
    window rather than a full day. Anything spanning half a day or more
    carries its dates. */
export function fmtWindowLabel(sinceNs, untilNs) {
  if (!sinceNs || !untilNs) return 'all time';
  const span = untilNs - sinceNs;
  const since = new Date(sinceNs / 1e6).toISOString();
  const until = new Date(untilNs / 1e6).toISOString();
  if (span >= 12 * 3600e9) {
    return `${since.slice(0, 10)} ${since.slice(11, 16)} – ${until.slice(0, 10)} ${until.slice(11, 16)}Z`;
  }
  return `${since.slice(11, 16)} – ${until.slice(11, 16)}Z`;
}

/** Uptime as "6 d 04:11", the shape the Server screen states it in. */
export function fmtUptime(ns) {
  const total = Math.floor(ns / 1e9);
  const days = Math.floor(total / 86400);
  const hours = String(Math.floor((total % 86400) / 3600)).padStart(2, '0');
  const minutes = String(Math.floor((total % 3600) / 60)).padStart(2, '0');
  return (days ? days + ' d ' : '') + hours + ':' + minutes;
}
