import React from 'react';
/* Hairline-row table. Numeric columns: align:'right', mono figures, tabular-nums.
   columns: [{key, label, align, mono, render, maxWidth}] */
export function DataTable({ columns = [], rows = [], density = 'comfortable', onRowClick, selectedIndex, sortKey, sortDir, onSort, style }) {
  const rowH = density === 'dense' ? 28 : 40;
  const cellPad = density === 'dense' ? '0 10px' : '0 12px';
  return <table style={{ width: '100%', borderCollapse: 'collapse', fontFamily: 'var(--font-sans)', fontSize: 'var(--text-13)', lineHeight: 'var(--lh-13)', ...style }}>
    <thead><tr>
      {columns.map((c) => <th key={c.key} onClick={onSort ? () => onSort(c.key) : undefined}
        style={{ textAlign: c.align || 'left', padding: cellPad, height: rowH - 8, color: 'var(--ink-muted)', fontWeight: 500, fontSize: 'var(--text-12)', lineHeight: 'var(--lh-12)', borderBottom: '1px solid var(--hairline)', cursor: onSort ? 'pointer' : 'default', whiteSpace: 'nowrap', userSelect: 'none' }}>
        {c.label}{sortKey === c.key ? <span style={{ color: 'var(--ink-faint)', marginLeft: 4 }}>{sortDir === 'asc' ? '↑' : '↓'}</span> : null}</th>)}
    </tr></thead>
    <tbody>
      {rows.map((r, i) => <tr key={i} onClick={onRowClick ? () => onRowClick(r, i) : undefined}
        onMouseEnter={(e) => { if (onRowClick) e.currentTarget.style.background = 'var(--bg-sunken)'; }}
        onMouseLeave={(e) => { e.currentTarget.style.background = selectedIndex === i ? 'var(--bg-sunken)' : 'transparent'; }}
        style={{ cursor: onRowClick ? 'pointer' : 'default', background: selectedIndex === i ? 'var(--bg-sunken)' : 'transparent' }}>
        {columns.map((c) => <td key={c.key} style={{ textAlign: c.align || 'left', padding: cellPad, height: rowH, borderBottom: '1px solid var(--hairline)', color: 'var(--ink)', fontFamily: c.mono ? 'var(--font-mono)' : 'inherit', fontVariantNumeric: 'tabular-nums', fontSize: c.mono ? 'var(--text-12)' : 'inherit', whiteSpace: 'nowrap', overflow: 'hidden', textOverflow: 'ellipsis', maxWidth: c.maxWidth || 'none' }}>
          {c.render ? c.render(r[c.key], r, i) : r[c.key]}</td>)}
      </tr>)}
    </tbody>
  </table>;
}
