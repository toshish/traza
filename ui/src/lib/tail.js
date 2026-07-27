// The live tail's client: consumes the server's admission stream.
//
// This replaces a polling state machine — watermark, cursor chain, dedupe set —
// and the replacement is much smaller because most of that machinery existed to
// fake something the protocol could not express. Polling `/v1/spans?since=` can
// only ask "what started after time T", which is not the question a tail asks.
// A span that ran longer than one poll interval starts before the watermark and
// arrives after it, so the server filtered it out forever. Every one of the
// old machine's bugs — the equal-timestamp burst that could not be paged past,
// the dedupe set that had to be pruned exactly at the watermark — was a symptom
// of that mismatch rather than a bug in the arithmetic.
//
// The server now assigns each admission a sequence number and streams. The
// cursor is one opaque token, positions are unique, and there is nothing to
// deduplicate: the server never sends the same position twice.
//
// What is left here is genuinely a client's job — framing, reconnection, and
// what to do when the server says a subscriber fell too far behind — and it is
// kept free of the DOM so it can be tested, which is how the last set of bugs
// was eventually found.

/** Shortest wait before reconnecting after a stream ends. */
export const RECONNECT_MIN_MS = 500;
/** Longest wait between reconnection attempts. */
export const RECONNECT_MAX_MS = 10_000;
/** Spans of history the tail opens with, so the screen is not blank. */
export const DEFAULT_BACKFILL = 200;

/** Exponential backoff, capped. */
export function backoffMs(attempt) {
  return Math.min(RECONNECT_MAX_MS, RECONNECT_MIN_MS * 2 ** Math.max(0, attempt));
}

/** Fresh client state. `cursor` is the server's opaque position token. */
export function newTailState() {
  return { cursor: null };
}

/** An incremental server-sent-events framer.
 *
 *  Returns a function that takes a chunk of text and yields whatever complete
 *  frames it completed. Chunk boundaries fall wherever TCP puts them, which is
 *  routinely mid-frame and occasionally mid-multi-byte-character, so the
 *  remainder has to be carried rather than parsed optimistically. */
export function createFramer() {
  let buffered = '';
  return (chunk) => {
    buffered += chunk;
    const frames = [];
    let at = buffered.indexOf('\n\n');
    while (at !== -1) {
      frames.push(buffered.slice(0, at));
      buffered = buffered.slice(at + 2);
      at = buffered.indexOf('\n\n');
    }
    return frames;
  };
}

/** Interprets one frame, or returns null for anything with no payload.
 *
 *  A frame starting with `:` is a comment — the heartbeat — and an unknown
 *  event name is ignored rather than treated as an error, so the server can add
 *  frame types without breaking a client that predates them. */
export function parseFrame(frame) {
  if (!frame || frame.startsWith(':')) return null;
  let event = 'message';
  const data = [];
  for (const line of frame.split('\n')) {
    if (line.startsWith('event:')) event = line.slice(6).trim();
    else if (line.startsWith('data:')) data.push(line.slice(5).trim());
  }
  if (!data.length) return null;
  let payload;
  try {
    payload = JSON.parse(data.join('\n'));
  } catch (e) {
    return null;
  }
  if (event === 'spans') {
    return { type: 'spans', spans: payload.spans || [], cursor: payload.cursor || null };
  }
  // A gap carries no position. It is a discontinuity: everything held before it
  // is void, and the stream resumes with a fresh backlog from the live edge.
  if (event === 'gap') {
    return { type: 'gap', missed: typeof payload.missed === 'number' ? payload.missed : null };
  }
  return null;
}

const defaultSleep = (ms) => new Promise((done) => setTimeout(done, ms));

/** Consumes the tail stream until `signal` aborts, reconnecting as needed.
 *
 *  `open(params)` resolves an async iterable of text chunks — injected so this
 *  can be driven by a generator in a test rather than by a socket.
 *
 *  Reconnection resumes from the last position rather than from the head, so a
 *  blip costs nothing as long as it is shorter than the server's ring. When it
 *  is not, the server says so and `onGap` is given the position to backfill up
 *  to; being told is what makes that recoverable, and it is the case the old
 *  polling client had no way to even detect. */
export async function runTail(state, options) {
  const {
    open,
    filter = {},
    backfill = DEFAULT_BACKFILL,
    onSpans,
    onGap,
    onStatus,
    signal,
    sleep = defaultSleep,
  } = options;

  let attempt = 0;
  while (!(signal && signal.aborted)) {
    try {
      const params = { ...filter };
      // Resuming is exact, so it asks for no history. Re-requesting the
      // backlog on every reconnect would replay the whole screen each time the
      // connection blinked.
      if (state.cursor) params.cursor = state.cursor;
      else if (backfill) params.backfill = backfill;

      const chunks = await open(params);
      if (onStatus) onStatus('live');

      const framer = createFramer();
      for await (const chunk of chunks) {
        if (signal && signal.aborted) return;
        for (const raw of framer(chunk)) {
          // Backoff resets on PROGRESS, not on a connection being accepted.
          // Resetting on the connection meant a server that accepted and then
          // immediately closed — a misconfigured proxy, a half-started
          // process — was reconnected to every 500ms indefinitely, because
          // each attempt "succeeded" before failing. A frame, including the
          // heartbeat, is the evidence that the stream actually works.
          attempt = 0;
          const frame = parseFrame(raw);
          if (!frame) continue;
          if (frame.type === 'spans') {
            // Delivered BEFORE the position advances. If delivery throws, the
            // reconnect resumes from the old position and sends these again —
            // a visible duplicate, where advancing first would have been an
            // invisible loss.
            if (frame.spans.length && onSpans) onSpans(frame.spans);
            state.cursor = frame.cursor;
          } else if (frame.type === 'gap') {
            // Drop the position: it names entries the server no longer has, and
            // there is no query that can fetch them — `/v1/spans` is ordered by
            // event time and cannot address an admission range at all. The
            // server restarts this subscription at the live edge, so the next
            // `spans` frame is a clean rebuild, and the consumer's job is to
            // discard what it was holding rather than to try to patch it.
            state.cursor = null;
            if (onGap) await onGap(frame.missed);
          }
        }
      }
    } catch (e) {
      if (signal && signal.aborted) return;
      // Any failure is a disconnection: the loop below reconnects. There is no
      // error state to show, because a tail that reconnects silently is the
      // behaviour a user expects from something labelled "live".
    }
    if (signal && signal.aborted) return;
    if (onStatus) onStatus('reconnecting');
    await sleep(backoffMs(attempt));
    attempt += 1;
  }
}

// Gap bookkeeping.
//
// This lived in the component as two `useState` calls, and both of its bugs
// lived there with it: `null` folded into a running total became zero, so an
// uncounted break rendered nothing; and rebuilding the subscription reset the
// counted half while leaving the uncounted half behind, so a new filter
// inherited the old one's warning. Neither was reachable by a test, because
// nothing but React could call it — the same reason the original polling bugs
// survived review.
//
// It is a value and three functions now. The component holds one piece of state
// and renders what `gapLabel` returns.

/** No gaps seen. */
export function newGapState() {
  return { missed: 0, uncounted: 0 };
}

/** Records one gap. `missed` is a number when the server could count the loss,
 *  and null when it could not — which happens on a restart, because sequence
 *  numbers are per-process and the old position is not comparable.
 *
 *  Counted and uncounted breaks are kept apart rather than summed. Folding an
 *  unknown into a total makes it zero, and zero renders as "nothing was lost",
 *  which is the opposite of what happened. */
export function recordGap(state, missed) {
  return typeof missed === 'number'
    ? { ...state, missed: state.missed + missed }
    : { ...state, uncounted: state.uncounted + 1 };
}

/** What to show, or null for nothing.
 *
 *  An uncounted break makes any accompanying number a FLOOR rather than a
 *  figure, so it is rendered as one. Reporting "5 missed" when an earlier break
 *  could not be counted states a precision the data does not have. */
export function gapLabel(state, format = String) {
  if (state.uncounted && state.missed) return `${format(state.missed)}+ missed`;
  if (state.uncounted) return 'spans missed';
  if (state.missed) return `${format(state.missed)} missed`;
  return null;
}
