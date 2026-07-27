import React from 'react';
import { api } from '../lib/api.js';
import { fmtClockNs, fmtDurationNs, fmtNum, fmtRate } from '../lib/format.js';
import { Card, Chip, LiveDot, Mono } from '../components/primitives/Chrome.jsx';
import {
  newTailState, runTail, newGapState, recordGap, recordDropped, gapLabel, gapDetail,
  newRateWindow, recordArrivals, ratePerSecond,
} from '../lib/tail.js';

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
// How long typing has to settle before the filter is applied. Each change
// tears down the streaming connection and opens another, so a filter bound
// straight to the input opened one connection per keystroke — eight for
// "checkout", each a request, a scan and a fresh backlog.
const FILTER_SETTLE_MS = 250;
// How often the rate window is re-evaluated when nothing is arriving. The
// window itself lives in lib/tail.js: it holds bucketed COUNTS, so its size is
// fixed by the window duration rather than by the traffic, and it has no rate
// ceiling. Averaging matters because spans arrive in bursts of whatever the SDK
// flushed — a per-frame rate swings between zero and enormous and reads as
// noise.
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
  // Gap bookkeeping lives in lib/tail.js, where it can be tested. Both of its
  // bugs — an unknown count folded to zero, and a reset that cleared only half
  // the state — survived because nothing but React could reach it.
  const [gapState, setGapState] = React.useState(newGapState);
  // `service` is what the input shows; `appliedService` is what the stream is
  // filtered by, and it trails the input until typing stops.
  const [service, setService] = React.useState('');
  const [appliedService, setAppliedService] = React.useState('');
  const [errorsOnly, setErrorsOnly] = React.useState(false);
  const buffer = React.useRef([]);
  // Rows carry a client-assigned id so React can keep them mounted. Their
  // primary key cannot serve: the same (trace, span) may legitimately be
  // admitted twice, once per update. The array index cannot either — it shifts
  // on every prepend, which changed every key and rebuilt all 300 rows per
  // frame.
  const nextRowId = React.useRef(0);
  const arrivals = React.useRef(newRateWindow());
  // `paused` is read inside a long-lived async loop, which closed over the
  // value it had when the stream opened. The ref is what the loop reads.
  const pausedRef = React.useRef(false);
  pausedRef.current = paused;

  React.useEffect(() => {
    if (service === appliedService) return undefined;
    const settle = setTimeout(() => setAppliedService(service), FILTER_SETTLE_MS);
    return () => clearTimeout(settle);
  }, [service, appliedService]);

  React.useEffect(() => {
    // One subscription per filter. Aborting is what ends the previous loop and
    // releases its connection — there is no "ignore the late response" case to
    // handle any more, because there is no response, only a stream that stops.
    const controller = new AbortController();
    const state = newTailState();
    setRows([]);
    setPending(0);
    // All of it: a warning earned under the previous filter has nothing to do
    // with this subscription.
    setGapState(newGapState());
    // Reset the DISPLAYED rate too, not just its inputs. Clearing the window
    // while leaving the number on screen left the previous filter's rate
    // standing over an empty table until something happened to arrive.
    setRate(null);
    buffer.current = [];
    arrivals.current = newRateWindow();

    const filter = {
      service: appliedService || undefined,
      status: errorsOnly ? 'error' : undefined,
    };

    // The rate is a sliding window, so it has to decay on its own. Computing it
    // only when spans arrive meant a burst that stopped kept showing its peak
    // rate indefinitely — the one reading that is certainly wrong.
    const decay = setInterval(() => {
      const now = Date.now();
      arrivals.current = recordArrivals(arrivals.current, 0, now);
      setRate(ratePerSecond(arrivals.current, now));
    }, RATE_TICK_MS);

    runTail(state, {
      open: (params) => api.tailChunks(params, controller.signal),
      filter,
      signal: controller.signal,
      onStatus: setStatus,
      onSpans: (spans) => {
        const now = Date.now();
        // One bucket update for the whole batch, not one entry per span: the
        // cost of measuring the rate must not scale with the rate.
        arrivals.current = recordArrivals(arrivals.current, spans.length, now);
        setRate(ratePerSecond(arrivals.current, now));

        // Newest first, matching the table's order.
        const newestFirst = spans
          .slice()
          .reverse()
          .map((span) => {
            nextRowId.current += 1;
            return { span, id: nextRowId.current };
          });
        if (pausedRef.current) {
          const combined = [...newestFirst, ...buffer.current];
          buffer.current = combined.slice(0, MAX_ROWS);
          // Overflow is discarded on purpose, and said out loud. Silently
          // dropping spans the reader believes they are queued to see is the
          // same hole a gap would be.
          setGapState((state) => recordDropped(state, combined.length - buffer.current.length));
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
        setGapState((state) => recordGap(state, count));
      },
    });

    return () => {
      controller.abort();
      clearInterval(decay);
    };
  }, [appliedService, errorsOnly]);

  const resume = () => {
    // Read the buffer OUT before clearing it. A `setRows` updater runs during
    // the later render, not at this call, so an updater that reads
    // `buffer.current` saw the empty array this function had already assigned —
    // and resuming produced no rows at all.
    //
    // The buffer is already newest-first — each batch prepends its own
    // newest-first block — so it goes straight on top.
    const held = buffer.current;
    buffer.current = [];
    setRows((all) => [...held, ...all].slice(0, MAX_ROWS));
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
        setGapState(newGapState());
      }}>Clear</Chip>
      {gapLabel(gapState, fmtNum) ? <Chip
        title={gapDetail(gapState)}
        style={{ background: 'var(--warn-tint)', borderColor: 'var(--warn)', color: 'var(--warn)' }}>
        {gapLabel(gapState, fmtNum)}
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
      {rows.map(({ span, id }, index) => {
        const error = span.status === 'error';
        return <div key={id}
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
