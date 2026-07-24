import React from 'react';
/* Skeleton = faint grid placeholder blocks, subtle pulse (opacity only). */
export function Skeleton({ lines = 3, height = 12, width, style }) {
  return <div style={{ display: 'flex', flexDirection: 'column', gap: 8, ...style }}>
    <style>{'@keyframes tz-skel{0%,100%{opacity:1}50%{opacity:0.55}}'}</style>
    {Array.from({ length: lines }).map((_, i) => <div key={i}
      style={{ height, width: width || (i === lines - 1 ? '60%' : '100%'), background: 'var(--grid)', borderRadius: 'var(--radius-control)', animation: 'tz-skel 1.6s cubic-bezier(0,0,0.2,1) infinite' }}></div>)}
  </div>;
}
