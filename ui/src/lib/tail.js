// The live tail's polling logic, extracted so it can be tested without a DOM.
//
// It lived inside the component, and two bugs hid there because nothing could
// call it: a watermark that could not advance past an equal-timestamp burst,
// and a page budget that discarded the cursor and so re-stranded the burst one
// budget further out. Both are properties of this state machine, not of the
// rendering, so this is where they belong.

/** A span's primary key, as one string.
 *
 *  Structured rather than concatenated with a separator: trace and span ids are
 *  arbitrary strings, so any delimiter can appear inside one — and reaching for
 *  a control byte to dodge that is how a literal NUL ends up in a source file,
 *  which makes git treat it as binary and hides it from diff, blame and grep. */
export const spanKey = (span) => JSON.stringify([span.trace_id, span.span_id]);

/** Rows per request while draining. */
export const PAGE = 200;
/** Pages one tick drains before yielding, so a burst does not block the UI.
    Whatever remains is carried as an unfinished cursor chain — a budget that
    DISCARDED the cursor merely moved the stranding threshold outwards. */
export const MAX_PAGES_PER_TICK = 5;

/** Fresh polling state. */
export function newTailState() {
  return {
    sinceNs: null,
    // An unfinished {cursor, base}, carried BETWEEN ticks.
    chain: null,
    seen: new Map(),
  };
}

/** Runs one poll tick against `fetchPage`, returning the spans not seen before.
 *
 *  `fetchPage(params)` resolves `{spans, next_cursor}` — the `/v1/spans`
 *  envelope. `state` is mutated; `now` is injectable so a test does not depend
 *  on the clock.
 *
 *  Three invariants make this correct rather than merely incremental:
 *
 *  - Paging is by cursor. A watermark cannot separate spans that share a
 *    timestamp, and an SDK flush routinely produces hundreds that do.
 *  - The watermark advances ONLY when a chain is exhausted. Moving it mid-chain
 *    re-reads the burst's prefix forever, because every span in an
 *    equal-timestamp burst is `>= since`.
 *  - The dedupe set retains exactly the keys ON the watermark, and is pruned
 *    only when the watermark moves. Evicting by size instead made a burst
 *    that cannot advance the watermark replay indefinitely.
 */
export async function pollOnce(state, fetchPage, { filter = {}, now = Date.now() } = {}) {
  const continuing = state.chain;
  // Continuing reuses the chain's ORIGINAL base: a cursor is only meaningful
  // against the filter it was issued for, and re-deriving `since` mid-chain
  // would move the floor underneath it.
  const base = continuing ? continuing.base : {
    limit: PAGE,
    ...filter,
    since: state.sinceNs ?? Math.round((now - 5000) * 1e6),
  };

  const fresh = [];
  let cursor = continuing ? continuing.cursor : null;
  let exhausted = false;
  for (let page = 0; page < MAX_PAGES_PER_TICK; page += 1) {
    // eslint-disable-next-line no-await-in-loop
    const answer = await fetchPage(cursor ? { ...base, cursor } : base);
    fresh.push(...(answer.spans || []));
    cursor = answer.next_cursor || null;
    if (!cursor) { exhausted = true; break; }
  }
  state.chain = exhausted ? null : { cursor, base };

  // One dedupe for every consumer. `since` is inclusive, so every span AT the
  // watermark returns on the next tick; the paused path used to skip this and
  // re-buffer a quiet page until it filled.
  //
  // `seen` maps key -> start time rather than being a plain set, because
  // pruning it correctly needs each key's timestamp and the spans themselves
  // are long gone by the next tick.
  const added = fresh.filter((span) => !state.seen.has(spanKey(span)));
  for (const span of added) state.seen.set(spanKey(span), span.start_time_ns);

  if (exhausted && fresh.length) {
    state.sinceNs = fresh[fresh.length - 1].start_time_ns;
    // Retain exactly the keys sitting ON the inclusive watermark; drop the
    // rest, which `since` can never return again.
    //
    // Evicting by SIZE is wrong here in a way that never settles: a burst
    // sharing one timestamp cannot advance the watermark, so its keys are
    // needed on every later poll — dropping them made 1,250 spans at one
    // timestamp replay 1000, 250, 1000, 250… forever. Pruning against only
    // the CURRENT tick's spans is wrong too, and was the first attempt at
    // this: the earlier ticks' keys are equally on the watermark, and
    // forgetting them replays exactly the prefix they covered.
    for (const [key, startNs] of state.seen) {
      if (startNs !== state.sinceNs) state.seen.delete(key);
    }
  } else if (!fresh.length && state.sinceNs == null) {
    state.sinceNs = Math.round(now * 1e6);
  }
  return added;
}
