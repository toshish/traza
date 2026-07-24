import React from 'react';
/* Loading = a thin indeterminate hairline bar in accent. No spinners. */
export function LoadingBar({ active = true, style }) {
  if (!active) return null;
  return <div style={{ position: 'relative', height: 2, background: 'var(--hairline)', overflow: 'hidden', borderRadius: 'var(--radius-bar)', ...style }}>
    <style>{'@keyframes tz-load{0%{left:-30%}100%{left:100%}}'}</style>
    <div style={{ position: 'absolute', top: 0, bottom: 0, width: '30%', background: 'var(--accent)', borderRadius: 'var(--radius-bar)', animation: 'tz-load 1.2s cubic-bezier(0,0,0.2,1) infinite' }}></div>
  </div>;
}
