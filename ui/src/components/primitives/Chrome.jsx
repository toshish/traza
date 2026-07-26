import React from 'react';

// The small shared surfaces every screen is built from. They exist as
// components rather than repeated inline styles so a change to "what a card
// is" happens once — the mockup is consistent about these to the pixel, and
// consistency is the thing that decays first when it is copy-pasted.

/** A hairline-bordered surface on raised paper. The system's only container:
    6px radius, 1px hairline, no shadow, ever. */
export function Card({ children, pad = '12px 14px', style, ...rest }) {
  return <div style={{
    background: 'var(--bg-raised)', border: '1px solid var(--hairline)',
    borderRadius: 'var(--radius-card)', padding: pad, minWidth: 0, ...style,
  }} {...rest}>{children}</div>;
}

/** The uppercase micro-label that titles every panel. */
export function Eyebrow({ children, style }) {
  return <h2 style={{
    margin: 0, fontSize: 12, textTransform: 'uppercase', letterSpacing: 'var(--tracking-caps)',
    fontWeight: 500, color: 'var(--ink-muted)', ...style,
  }}>{children}</h2>;
}

/** A panel header: eyebrow on the left, optional note, optional action right. */
export function PanelHead({ title, note, action, style }) {
  return <div style={{ display: 'flex', alignItems: 'baseline', gap: 10, marginBottom: 10, ...style }}>
    <Eyebrow>{title}</Eyebrow>
    {note ? <span style={{ fontSize: 12, color: 'var(--ink-faint)' }}>{note}</span> : null}
    {action ? <span style={{ marginLeft: 'auto' }}>{action}</span> : null}
  </div>;
}

/** A bordered control: the system's button, in three weights. Never a pill —
    3px radius, hairline border, no shadow. */
export function Chip({ children, active, tone = 'default', mono, dashed, onClick, title, style }) {
  const [hover, setHover] = React.useState(false);
  const primary = tone === 'primary';
  const background = primary ? (hover ? 'var(--accent-hover)' : 'var(--accent)')
    : active ? 'var(--accent-tint)' : hover ? 'var(--bg-sunken)' : 'transparent';
  const color = primary ? '#FFFFFF' : active ? 'var(--accent-hover)' : 'var(--ink-muted)';
  return <span onClick={onClick} title={title}
    onMouseEnter={() => setHover(true)} onMouseLeave={() => setHover(false)}
    role={onClick ? 'button' : undefined} tabIndex={onClick ? 0 : undefined}
    onKeyDown={onClick ? (e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); onClick(e); } } : undefined}
    style={{
      display: 'inline-flex', alignItems: 'center', gap: 6, fontSize: 12,
      fontWeight: primary ? 500 : 400, padding: '3px 10px',
      border: primary ? 'none' : `1px ${dashed ? 'dashed' : 'solid'} ${active ? 'var(--accent)' : 'var(--hairline)'}`,
      borderRadius: 'var(--radius-control)', background, color,
      fontFamily: mono ? 'var(--font-mono)' : 'inherit',
      cursor: onClick ? 'pointer' : 'default', whiteSpace: 'nowrap', userSelect: 'none', ...style,
    }}>{children}</span>;
}

/** A keyboard key, drawn as one. */
export function Kbd({ children, style }) {
  return <span style={{
    fontFamily: 'var(--font-mono)', fontSize: 11, border: '1px solid var(--hairline)',
    borderRadius: 'var(--radius-control)', padding: '0 4px', color: 'var(--ink-faint)',
    whiteSpace: 'nowrap', ...style,
  }}>{children}</span>;
}

/** A measured figure: mono, tabular, accent. The one place terracotta is
    allowed, because a measurement is exactly what it marks. */
export function Figure({ value, unit, size = 24, color = 'var(--accent)', style }) {
  return <span style={{ display: 'inline-flex', alignItems: 'baseline', gap: 5, ...style }}>
    <span style={{
      fontFamily: 'var(--font-mono)', fontSize: size, lineHeight: size > 16 ? '30px' : '20px',
      fontWeight: 500, fontVariantNumeric: 'tabular-nums', color,
    }}>{value}</span>
    {unit ? <span style={{ fontFamily: 'var(--font-mono)', fontSize: 12, color: 'var(--ink-muted)' }}>{unit}</span> : null}
  </span>;
}

/** Mono inline text — ids, keys, code fragments inside prose. */
export function Mono({ children, color = 'var(--ink)', size = 12, style }) {
  return <span style={{ fontFamily: 'var(--font-mono)', fontSize: size, color, ...style }}>{children}</span>;
}

/** The live dot: a pulse that says data is still arriving. */
export function LiveDot({ color = 'var(--ok)', style }) {
  return <span aria-hidden="true" style={{
    width: 5, height: 5, borderRadius: '50%', background: color,
    animation: 'tzpulse 2.4s ease-in-out infinite', flex: 'none', ...style,
  }} />;
}

/** The loading hairline: a thin accent sweep, never a spinner. */
export function LoadingBar({ active, style }) {
  return <div aria-hidden={!active} style={{
    position: 'relative', height: 2, overflow: 'hidden', background: 'transparent', ...style,
  }}>
    {active ? <div style={{
      position: 'absolute', top: 0, height: 2, width: '30%', background: 'var(--accent)',
      borderRadius: 1, animation: 'tzload 1.1s var(--ease-out) infinite',
    }} /> : null}
  </div>;
}

/** A row of range buttons — the window switcher every screen shares. */
export function RangeBar({ ranges, active, onPick, style }) {
  return <div style={{ display: 'flex', alignItems: 'center', gap: 6, ...style }}>
    {ranges.map((r) => <Chip key={r.id} mono active={active === r.id} onClick={() => onPick(r.id)}>{r.label}</Chip>)}
  </div>;
}

/** Empty and error states. Both say what happened and what to do next —
    a dead end with no next step is the most expensive screen in a tool. */
export function EmptyState({ message, command, style }) {
  return <div style={{
    border: '1px dashed var(--hairline)', borderRadius: 'var(--radius-card)',
    padding: '20px 22px', color: 'var(--ink-muted)', fontSize: 13, lineHeight: '20px', ...style,
  }}>
    <div style={{ textWrap: 'pretty' }}>{message}</div>
    {command ? <pre style={{
      margin: '10px 0 0', padding: '10px 12px', background: 'var(--bg-sunken)',
      borderRadius: 'var(--radius-control)', fontFamily: 'var(--font-mono)', fontSize: 12,
      lineHeight: '18px', color: 'var(--ink)', overflowX: 'auto', whiteSpace: 'pre-wrap',
    }}>{command}</pre> : null}
  </div>;
}

export function ErrorState({ what, next, onRetry, style }) {
  return <div role="alert" style={{
    border: '1px solid var(--error)', background: 'var(--error-tint)',
    borderRadius: 'var(--radius-card)', padding: '12px 14px', fontSize: 13, lineHeight: '20px', ...style,
  }}>
    <div style={{ color: 'var(--ink)', fontWeight: 500 }}>{what}</div>
    {next ? <div style={{ color: 'var(--ink-muted)', marginTop: 3 }}>{next}</div> : null}
    {onRetry ? <div style={{ marginTop: 8 }}><Chip onClick={onRetry}>Retry</Chip></div> : null}
  </div>;
}

/** A skeleton block, for the first paint before anything has arrived. */
export function Skeleton({ height = 14, width = '100%', style }) {
  return <div aria-hidden="true" style={{
    height, width, background: 'var(--bg-sunken)', borderRadius: 'var(--radius-control)',
    animation: 'tzpulse 1.6s ease-in-out infinite', ...style,
  }} />;
}
