import React from 'react';
import { api } from '../lib/api.js';
import { fmtClockNs, fmtDurationNs, fmtNum, fmtRate } from '../lib/format.js';
import { Card, Chip, LiveDot, Mono } from '../components/primitives/Chrome.jsx';
import { newTailState, runTail } from '../lib/tail.js';

// Spans as they land — over one held connection, not a poll.
//
// The screen used to ask `/v1/spans?since=<watermark>` every 1.5 seconds, and
// that could not express what this screen claims to show. A span outliving one
// tick starts before the watermark and arrives after it, so the server dropped
// it permanently. The server now streams in admission order, which is the order
// "as they land" actually means.
//
// The network cost went the same direction: an idle tail was forty round trips
// a minute returning nothing, and is now zero — one parked connection and a
// heartbeat every fifteen seconds.

const MAX_ROWS = 300;
// Arrival rate is averaged over a sliding window rather than computed per
// frame. Spans arrive in bursts of whatever the SDK flushed, so a per-frame
// rate is a number that swings between zero and enormous and reads as noise.
const RATE_WINDOW_MS = 5_000;
// How often the window is re-evaluated when nothing is arriving.
const RATE_TICK_MS = 1_000;

export function TailScreen({ go }) {
  const [rows, setRows] = React.useState([]);
  const [paused, setPaused] = React.useState(false);
  const [pending, setPending] = React.useState(0);
  const [rate, setRate] = React.useState(null);
  const [status, setStatus] = React.useState('live');
  // Admissions the server dropped before this client could read them. Shown
  // rather than swallowed: the view is complete from the break onwards, and
  // saying so is the difference between a gap and a silent hole.
  // Two separate facts. `missed` is how many admissions were lost where the
  // server could count them; `gaps` is how many discontinuities happened at
  // all. They come apart on a server restart: sequence numbers are per-process,
  // so the count is genuinely unknowable and arrives as null — and treating
  // null as zero showed no warning while silently clearing the screen, which is
  // exactly the invisible discontinuity this feature exists to remove.
  const [missed, setMissed] = React.useState(0);
  const [gaps, setGaps] = React.useState(0);
  const [service, setService] = React.useState('');
  const [errorsOnly, setErrorsOnly] = React.useState(false);
  const buffer = React.useRef([]);
  const arrivals = React.useRef([]);
  // `paused` is read inside a long-lived async loop, which closed over the
  // value it had when the stream opened. The ref is what the loop reads.
  const pausedRef = React.useRef(false);
  pausedRef.current = paused;

  React.useEffect(() => {
    // One subscription per filter. Aborting is what ends the previous loop and
    // releases its connection — there is no "ignore the late response" case to
    // handle any more, because there is no response, only a stream that stops.
    const controller = new AbortController();
    const state = newTailState();
    setRows([]);
    setPending(0);
    // Reset the DISPLAYED rate too, not just its inputs. Clearing the window
    // while leaving the number on screen left the previous filter's rate
    // standing over an empty table until something happened to arrive.
    setRate(null);
    setMissed(0);
    buffer.current = [];
    arrivals.current = [];

    const filter = {
      service: service || undefined,
      status: errorsOnly ? 'error' : undefined,
    };

    // The rate is a sliding window, so it has to decay on its own. Computing it
    // only when spans arrive meant a burst that stopped kept showing its peak
    // rate indefinitely — the one reading that is certainly wrong.
    const decay = setInterval(() => {
      const now = Date.now();
      arrivals.current = arrivals.current.filter((at) => now - at < RATE_WINDOW_MS);
      setRate(arrivals.current.length ? (arrivals.current.length * 1000) / RATE_WINDOW_MS : 0);
    }, RATE_TICK_MS);

    runTail(state, {
      open: (params) => api.tailChunks(params, controller.signal),
      filter,
      signal: controller.signal,
      onStatus: setStatus,
      onSpans: (spans) => {
        const now = Date.now();
        arrivals.current = arrivals.current
          .filter((at) => now - at < RATE_WINDOW_MS)
          .concat(spans.map(() => now));
        setRate((arrivals.current.length * 1000) / RATE_WINDOW_MS);

        // Newest first, matching the table's order.
        const newestFirst = spans.slice().reverse();
        if (pausedRef.current) {
          buffer.current = [...newestFirst, ...buffer.current].slice(0, MAX_ROWS);
          setPending(buffer.current.length);
        } else {
          setRows((all) => [...newestFirst, ...all].slice(0, MAX_ROWS));
        }
      },
      onGap: (count) => {
        // The stream fell further behind than the server retains. There is
        // nothing to fetch: the dropped entries are exactly the ones no longer
        // addressable, and `/v1/spans` is ordered by event time, so it cannot
        // name an admission range at all. An earlier version queried it anyway
        // and prepended the result, which overlapped the entries the stream
        // then replayed and showed them twice.
        //
        // So the view is discarded and the stream rebuilds it from the live
        // edge. A visible break beats a plausible-looking splice.
        buffer.current = [];
        setPending(0);
        setRows([]);
        setGaps((total) => total + 1);
        if (typeof count === 'number') setMissed((total) => total + count);
      },
    });

    return () => {
      controller.abort();
      clearInterval(decay);
    };
  }, [service, errorsOnly]);

  const resume = () => {
    // The buffer is already newest-first — each tick prepends its own
    // newest-first batch — so reversing it here handed back an oldest-first
    // block on top of a newest-first list.
    setRows((all) => [...buffer.current, ...all].slice(0, MAX_ROWS));
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
        setMissed(0);
        setGaps(0);
      }}>Clear</Chip>
      {gaps ? <Chip
        title={missed
          ? 'The stream fell further behind than the server retains, so these spans never reached this view. They are still in the store — search for them on Traces.'
          : 'The stream broke and the view was rebuilt from the live edge. The server could not say how many spans were missed — a restart renumbers the stream, so the count is not comparable. Anything missing is still in the store; search for it on Traces.'}
        style={{ background: 'var(--warn-tint)', borderColor: 'var(--warn)', color: 'var(--warn)' }}>
        {missed ? `${fmtNum(missed)} missed` : 'spans missed'}
      </Chip> : null}
      <span style={{ marginLeft: 'auto', display: 'flex', alignItems: 'center', gap: 8, fontSize: 12, color: 'var(--ink-muted)' }}>
        <LiveDot color={paused ? 'var(--ink-faint)'
          : status === 'reconnecting' ? 'var(--warn)' : 'var(--ok)'} />
        {paused ? 'paused' : status}
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
        {['started', 'service', 'name', 'trace', 'duration', 'status'].map((label, i) => (
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
