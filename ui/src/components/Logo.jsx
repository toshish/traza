import React from 'react';
/* The Traza mark: four offset horizontal bars — a descending waterfall whose
   extended top bar reads as the crossbar of a lowercase t. From the design
   system's assets/logo.svg; terracotta works on both themes. */
export function Logo({ size = 20, style }) {
  return <svg width={size} height={size} viewBox="0 0 48 48" fill="var(--accent)" style={style} aria-hidden="true">
    <rect x="2" y="10" width="30" height="5" rx="1.5"></rect>
    <rect x="16" y="19" width="22" height="5" rx="1.5"></rect>
    <rect x="20" y="28" width="16" height="5" rx="1.5"></rect>
    <rect x="24" y="37" width="20" height="5" rx="1.5"></rect>
  </svg>;
}
