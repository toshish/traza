import React from 'react';

// The Traza mark and wordmark, from the design system's Brand cards.
//
// The mark is the drawn line the whole system is built on: offset bars
// descending as a waterfall off a stem, which together read as a lowercase
// "t". The stem is what changed in the latest revision — the earlier mark was
// four bars with nothing holding them, and did not resolve as a letter.
//
// Inlined rather than loaded through `<img>` on purpose. The design system is
// explicit about it: an img-referenced SVG cannot inherit page color, so the
// reversed and monochrome variants only work when the markup is in the
// document. That is also why `tone` defaults to `currentColor` rather than
// hard-coding the accent — a mark on an inked chip has to invert with it.

/** Below this the four bars stop resolving as separate rows; the design
    system's guidance is to use the favicon variant instead.
    It is also the DEFAULT size — a floor that components have to remember to
    respect is one the navigation had already broken by rendering at 19. */
export const MARK_MIN_PX = 20;

/** The mark. `tone` accepts any CSS color; the default inherits, which is what
    makes the reversed lockup work without a second asset. */
export function Logo({ size = MARK_MIN_PX, tone = 'currentColor', title, style }) {
  return <svg width={size} height={size} viewBox="0 0 48 48" fill={tone}
    role={title ? 'img' : undefined} aria-hidden={title ? undefined : 'true'}
    aria-label={title} style={style}>
    {title ? <title>{title}</title> : null}
    {/* stem — the bar that makes the mark a letter rather than a chart */}
    <rect x="13" y="6" width="5" height="32" rx="1.5" />
    <rect x="3" y="13" width="27" height="5" rx="1.5" />
    <rect x="17" y="21" width="21" height="5" rx="1.5" />
    <rect x="21" y="29" width="15" height="5" rx="1.5" />
    <rect x="25" y="37" width="19" height="5" rx="1.5" />
  </svg>;
}

/** Mark plus wordmark. Lowercase always, JetBrains Mono 500, tracking -0.02em
    — the three things the brand card pins down about the wordmark. */
export function Lockup({ size = MARK_MIN_PX, tone = 'var(--accent)', textTone = 'var(--ink)', gap = 9, style }) {
  return <span style={{ display: 'inline-flex', alignItems: 'center', gap, ...style }}>
    <Logo size={size} tone={tone} />
    <span style={{
      fontFamily: 'var(--font-mono)',
      fontSize: Math.round(size * 0.79),
      fontWeight: 500,
      letterSpacing: '-0.02em',
      color: textTone,
    }}>traza</span>
  </span>;
}
