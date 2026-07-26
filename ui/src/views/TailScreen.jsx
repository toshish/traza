import React from 'react';
import { api } from '../lib/api.js';
import { usePoll } from '../lib/route.js';
import { fmtClockNs, fmtDurationNs, fmtNum, fmtRate } from '../lib/format.js';
import { Card, Chip, LiveDot, Mono } from '../components/primitives/Chrome.jsx';
import { newTailState, pollOnce } from '../lib/tail.js';

// Spans as they land. The old table was a snapshot with a Refresh button,
// which is the wrong shape for data that arrives continuously.
//
// Polling is incremental: each tick asks only for spans newer than the last
// one seen, so the cost of watching is proportional to what arrived rather
// than to what is on screen. A paused tail stops asking entirely.
//
// Two invariants make it correct rather than merely incremental. Paging is by
// cursor, because a watermark cannot separate spans that share a timestamp and
// an SDK flush routinely produces hundreds that do. And the watermark advances
// only when a cursor chain is exhausted, because moving it mid-chain re-reads
// the burst's prefix forever and never reaches its end.

const MAX_ROWS = 300;
const TICK_MS = 1500;

export function TailScreen({ go }) {
  const [rows, setRows] = React.useState([]);
  const [paused, setPaused] = React.useState(false);
  const [pending, setPending] = React.useState(0);
  const [rate, setRate] = React.useState(null);
  const [service, setService] = React.useState('');
  const [errorsOnly, setErrorsOnly] = React.useState(false);
  const buffer = React.useRef([]);
  const lastTick = React.useRef(null);
  const inFlight = React.useRef(false);
  // The polling state machine lives in lib/tail.js so it can be tested without
  // a DOM — which is where its two real bugs were found.
  const tail = React.useRef(newTailState());

  // Reset when the filter changes: a narrower tail should show what is arriving
  // now, not resume a cursor chain belonging to the wider one.
  React.useEffect(() => {
    tail.current = newTailState();
    buffer.current = [];
    setRows([]);
    setPending(0);
  }, [service, errorsOnly]);

  usePoll(async () => {
    // Ticks must not overlap. A slow response used to let the next tick start
    // against the same watermark, so both pages landed and every span appeared
    // twice.
    if (inFlight.current) return;
    inFlight.current = true;
    try {
      const added = await pollOnce(
        tail.current,
        (params) => api.spans(params),
        {
          filter: {
            service: service || undefined,
            status: errorsOnly ? 'error' : undefined,
          },
          maxRows: MAX_ROWS,
        },
      );

      const now = performance.now();
      if (lastTick.current) {
        const seconds = Math.max(0.001, (now - lastTick.current) / 1000);
        setRate(added.length / seconds);
      }
      lastTick.current = now;
      if (!added.length) return;

      const newestFirst = added.slice().reverse();
      if (paused) {
        buffer.current = [...newestFirst, ...buffer.current].slice(0, MAX_ROWS);
        setPending(buffer.current.length);
      } else {
        setRows((all) => [...newestFirst, ...all].slice(0, MAX_ROWS));
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
      <Chip onClick={() => {
        setRows([]);
        buffer.current = [];
        setPending(0);
      }}>Clear</Chip>
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
