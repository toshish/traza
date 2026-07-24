import React from 'react';
import { Button } from '../primitives/Button.jsx';
/* Filter bar: removable chips (service/name/attr) + duration & time-range inputs. */
export function FilterBar({ chips = [], onRemoveChip, onSearch, children, style }) {
  return <div style={{ display: 'flex', alignItems: 'center', gap: 8, flexWrap: 'wrap', padding: '8px 0', fontFamily: 'var(--font-sans)', ...style }}>
    {chips.map((c, i) => <span key={i} style={{ display: 'inline-flex', alignItems: 'center', gap: 6, background: 'var(--bg-sunken)', border: '1px solid var(--hairline)', borderRadius: 'var(--radius-control)', padding: '2px 4px 2px 8px', fontSize: 'var(--text-12)', lineHeight: 'var(--lh-12)' }}>
      <span style={{ color: 'var(--ink-muted)' }}>{c.field}</span>
      <span style={{ fontFamily: 'var(--font-mono)', color: 'var(--ink)', fontVariantNumeric: 'tabular-nums' }}>{c.op || '='}{c.value}</span>
      {onRemoveChip ? <button onClick={() => onRemoveChip(c, i)} style={{ border: 'none', background: 'transparent', color: 'var(--ink-faint)', cursor: 'pointer', padding: '0 2px', fontSize: 12, lineHeight: 1 }}>×</button> : null}
    </span>)}
    {children}
    {onSearch ? <Button variant="primary" size="sm" onClick={onSearch}>Search</Button> : null}
  </div>;
}
