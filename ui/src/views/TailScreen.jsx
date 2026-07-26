import React from 'react';
import { api } from '../lib/api.js';
import { usePoll } from '../lib/route.js';
import { fmtClockNs, fmtDurationNs, fmtNum, fmtRate } from '../lib/format.js';
import { Card, Chip, LiveDot, Mono } from '../components/primitives/Chrome.jsx';

// Spans as they land. The old table was a snapshot with a Refresh button,
// which is the wrong shape for data that arrives continuously.
//
// Polling is incremental: each tick asks only for spans newer than the last
// one seen, so the cost of watching is proportional to what arrived rather
// than to what is on screen. A paused tail stops asking entirely.

const MAX_ROWS = 300;
const TICK_MS = 1500;
/** Rows per request while draining a tick. */
const PAGE = 200;
/** Pages one tick will drain before yielding. A burst larger than this is
    picked up by the next tick rather than blocking this one — the tail stays
    responsive, and the cursor means nothing is lost in between. */
const MAX_PAGES_PER_TICK = 5;

/** A span's primary key, as one string.
 *
 *  Structured rather than concatenated with a separator: trace and span ids
 *  are arbitrary strings, so any delimiter can appear inside one — and
 *  reaching for a control byte to dodge that is how a literal NUL ends up in
 *  a source file, which makes git treat it as binary and hides it from diff,
 *  blame and grep. */
const spanKey = (span) => JSON.stringify([span.trace_id, span.span_id]);

export function TailScreen({ go }) {
  const [rows, setRows] = React.useState([]);
  const [paused, setPaused] = React.useState(false);
  const [pending, setPending] = React.useState(0);
  const [rate, setRate] = React.useState(null);
  const [service, setService] = React.useState('');
  const [errorsOnly, setErrorsOnly] = React.useState(false);
  const since = React.useRef(null);
  const buffer = React.useRef([]);
  const lastTick = React.useRef(null);
  const inFlight = React.useRef(false);

  // Reset the watermark when the filter changes: a narrower tail should show
  // what is arriving now, not resume from where the wider one stopped.
  React.useEffect(() => {
    since.current = null;
    buffer.current = [];
    setRows([]);
    setPending(0);
  }, [service, errorsOnly]);

  usePoll(async () => {
    // Ticks must not overlap. A slow response used to let the next tick start
    // against the same watermark, so both pages landed and the tail showed
    // every span twice.
    if (inFlight.current) return;
    inFlight.current = true;
    try {
      const base = {
        limit: PAGE,
        service: service || undefined,
        status: errorsOnly ? 'error' : undefined,
        // First tick starts from now rather than the beginning of the store:
        // a tail is about what is happening, and replaying a corpus into it
        // would bury that under history.
        since: since.current ?? Math.round((Date.now() - 5000) * 1e6),
      };

      // Drain by CURSOR, not by advancing the watermark past the newest
      // timestamp seen. Bumping to max(start)+1 silently skipped every span
      // that shared the last timestamp of a full page — and spans batched by
      // an SDK routinely share one. The cursor carries the full ordering key,
      // so a page boundary inside a timestamp resumes exactly where it left.
      const fresh = [];
      let cursor = null;
      for (let page = 0; page < MAX_PAGES_PER_TICK; page += 1) {
        // eslint-disable-next-line no-await-in-loop
        const answer = await api.spans(cursor ? { ...base, cursor } : base);
        const batch = answer.spans || [];
        fresh.push(...batch);
        cursor = answer.next_cursor || null;
        if (!cursor) break;
      }

      if (fresh.length) {
        // The watermark moves to the last span in stable order, and the next
        // tick re-asks from there with `since`; the cursor covers within-tick
        // paging, `since` covers between ticks.
        since.current = fresh[fresh.length - 1].start_time_ns;
      } else if (since.current == null) {
        since.current = Math.round(Date.now() * 1e6);
      }

      const now = performance.now();
      if (lastTick.current) {
        const seconds = Math.max(0.001, (now - lastTick.current) / 1000);
        setRate(fresh.length / seconds);
      }
      lastTick.current = now;
      if (!fresh.length) return;

      if (paused) {
        buffer.current = [...fresh.slice().reverse(), ...buffer.current].slice(0, MAX_ROWS);
        setPending(buffer.current.length);
      } else {
        setRows((all) => {
          // `since` is inclusive, so the span the watermark points at comes
          // back on the next tick. Dropping by primary key is what keeps it
          // from appearing twice.
          const already = new Set(all.map(spanKey));
          const added = fresh.filter((s) => !already.has(spanKey(s)));
          return [...added.reverse(), ...all].slice(0, MAX_ROWS);
        });
      }
    } catch (e) {
      /* a dropped tick is the next tick's problem */
    } finally {
      inFlight.current = false;
    }
  }, TICK_MS, true);

  const resume = () => {
    setRows((all) => [...buffer.current.reverse(), ...all].slice(0, MAX_ROWS));
    buffer.current = [];
    setPending(0);
    setPaused(false);
  };

  return <div style={{ display: 'grid', gap: 12, maxWidth: 1560 }}>
    <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap' }}>
      <Chip active={!paused} onClick={() => (paused ? resume() : setPaused(true))}>
        {paused ? '▶ Resume' : '❚❚ Pause'}
      </Chip>
      {paused && pending ? <Chip tone="primary" onClick={resume}>{fmtNum(pending)} new spans</Chip> : null}
      <Chip active={errorsOnly} onClick={() => setErrorsOnly((on) => !on)}>errors only</Chip>
      <input value={service} onChange={(e) => setService(e.target.value)} placeholder="service"
        aria-label="Filter by service"
        style={{
          padding: '3px 8px', border: '1px solid var(--hairline)', borderRadius: 'var(--radius-control)',
          background: 'var(--bg)', fontFamily: 'var(--font-mono)', fontSize: 12, color: 'var(--ink)',
          outline: 'none', width: 160,
        }} />
      <Chip onClick={() => { setRows([]); buffer.current = []; setPending(0); }}>Clear</Chip>
      <span style={{ marginLeft: 'auto', display: 'flex', alignItems: 'center', gap: 8, fontSize: 12, color: 'var(--ink-muted)' }}>
        <LiveDot color={paused ? 'var(--ink-faint)' : 'var(--ok)'} />
        {paused ? 'paused' : 'live'}
        <span style={{ color: 'var(--ink-faint)' }}>·</span>
        <Mono color="var(--accent)">{rate == null ? '—' : fmtRate(rate)}</Mono>
        <span style={{ color: 'var(--ink-faint)' }}>·</span>
        <span>{fmtNum(rows.length)} shown</span>
      </span>
    </div>

    <Card pad="0" style={{ overflow: 'hidden' }}>
      <div style={{
        display: 'grid', gridTemplateColumns: '104px 132px 1fr 100px 88px 70px',
        background: 'var(--bg-sunken)', borderBottom: '1px solid var(--hairline)',
      }}>
        {['arrived', 'service', 'name', 'trace', 'duration', 'status'].map((label, i) => (
          <div key={label} style={{
            padding: '6px 10px', fontSize: 12, fontWeight: 500, color: 'var(--ink-muted)',
            textAlign: i === 4 ? 'right' : 'left', whiteSpace: 'nowrap',
          }}>{label}</div>
        ))}
      </div>
      {rows.length === 0 ? <div style={{ padding: '18px 14px', fontSize: 13, color: 'var(--ink-muted)' }}>
        Waiting for spans. Anything ingested from now on appears here.
      </div> : null}
      {rows.map((span, index) => {
        const error = span.status === 'error';
        return <div key={span.trace_id + span.span_id + index}
          onClick={() => go(['trace', span.trace_id], { span: span.span_id })}
          role="link" tabIndex={0}
          onKeyDown={(e) => { if (e.key === 'Enter') go(['trace', span.trace_id], { span: span.span_id }); }}
          style={{
            display: 'grid', gridTemplateColumns: '104px 132px 1fr 100px 88px 70px',
            alignItems: 'center', borderBottom: '1px solid var(--hairline)',
            cursor: 'pointer', minHeight: 'var(--row-h)',
            // The newest row is tinted so the eye can find the edge of the
            // stream without reading timestamps.
            background: index === 0 && !paused ? 'var(--accent-tint)' : 'transparent',
          }}>
          <Cell mono muted>{fmtClockNs(span.start_time_ns)}</Cell>
          <Cell>{span.service}</Cell>
          <Cell>{span.name}</Cell>
          <Cell mono muted>{span.trace_id.slice(0, 10)}</Cell>
          <Cell mono align="right">{fmtDurationNs(span.end_time_ns - span.start_time_ns)}</Cell>
          <Cell color={error ? 'var(--error)' : 'var(--ink-muted)'}>{span.status || '—'}</Cell>
        </div>;
      })}
    </Card>
  </div>;
}

function Cell({ children, mono, muted, align, color }) {
  return <div style={{
    padding: 'var(--row-py) 10px',
    fontFamily: mono ? 'var(--font-mono)' : 'inherit',
    fontSize: mono ? 12 : 'var(--cell-fs)',
    fontVariantNumeric: mono ? 'tabular-nums' : undefined,
    color: color || (muted ? 'var(--ink-muted)' : 'var(--ink)'),
    textAlign: align || 'left',
    overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
  }}>{children}</div>;
}
