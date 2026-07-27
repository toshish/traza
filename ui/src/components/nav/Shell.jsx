import React from 'react';
import { fmtNum, fmtRate } from '../../lib/format.js';
import { Kbd, LiveDot } from '../primitives/Chrome.jsx';
import { Sparkbar } from '../charts/Marks.jsx';
import { Lockup } from '../Logo.jsx';

// The frame every screen sits in. Four top tabs could not hold seventeen
// screens, so navigation is a left rail grouped by the question you arrived
// with — "what happened", "how much", "how good", "is it healthy" — and the
// content area is full-bleed so a waterfall gets the pixels it needs.

/** The rail's groups, in the order the review argued for. */
export const NAV = [
  { label: 'explore', items: [
    { id: 'overview', label: 'Overview' },
    { id: 'traces', label: 'Traces' },
    { id: 'sessions', label: 'Sessions' },
    { id: 'tail', label: 'Live tail' },
  ] },
  { label: 'measure', items: [
    { id: 'analytics', label: 'Analytics' },
    { id: 'latency', label: 'Latency' },
    { id: 'failures', label: 'Failures' },
  ] },
  { label: 'evaluate', items: [
    { id: 'scores', label: 'Scores' },
    { id: 'experiments', label: 'Experiments' },
    { id: 'datasets', label: 'Datasets' },
  ] },
  { label: 'operate', items: [
    { id: 'server', label: 'Server' },
    { id: 'store', label: 'Store' },
    { id: 'connect', label: 'Connect' },
  ] },
];

/** Every screen's title and subtitle, so the header reads the same way
    wherever you are and a new screen cannot forget to say what it is. */
export const SCREENS = {
  overview: ['Overview', 'what changed since yesterday'],
  traces: ['Traces', 'every predicate the store understands'],
  trace: ['Trace', 'one run, on a time axis'],
  sessions: ['Sessions', 'conversations, most recent first'],
  session: ['Session', 'one conversation end to end'],
  conversation: ['Conversation', 'the turns, as they were exchanged'],
  analytics: ['Analytics', 'cost, tokens, latency and efficiency by any grouping'],
  latency: ['Latency', 'the distribution and the traces behind its tail'],
  failures: ['Failures', 'errors grouped by signature'],
  scores: ['Scores', 'annotations across traces, human and eval'],
  experiments: ['Experiments', 'two cohorts on score, cost and latency'],
  datasets: ['Datasets', 'a saved search promoted to an eval set'],
  tail: ['Live tail', 'spans as they land'],
  compare: ['Trace compare', 'a good run beside a bad one'],
  server: ['Server', 'what this process has actually done'],
  store: ['Store', 'segments, durability, export'],
  connect: ['Connect', 'point something at this server'],
};

function NavItem({ item, active, badge, onGo }) {
  const [hover, setHover] = React.useState(false);
  return <div onClick={() => onGo(item.id)} role="link" tabIndex={0}
    aria-current={active ? 'page' : undefined}
    onKeyDown={(e) => { if (e.key === 'Enter') onGo(item.id); }}
    onMouseEnter={() => setHover(true)} onMouseLeave={() => setHover(false)}
    style={{
      display: 'flex', alignItems: 'center', gap: 8, padding: '5px 16px 5px 14px',
      borderLeft: `2px solid ${active ? 'var(--accent)' : 'transparent'}`,
      background: active ? 'var(--accent-tint)' : hover ? 'var(--bg-sunken)' : 'transparent',
      color: active ? 'var(--ink)' : 'var(--ink-muted)', fontWeight: active ? 500 : 400,
      cursor: 'pointer', userSelect: 'none', fontSize: 13,
    }}>
    <span style={{ flex: 1, minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
      {item.label}
    </span>
    {badge ? <span style={{
      fontFamily: 'var(--font-mono)', fontSize: 11, fontVariantNumeric: 'tabular-nums',
      color: badge.tone === 'error' ? 'var(--error)' : 'var(--ink-faint)',
    }}>{badge.text}</span> : null}
  </div>;
}

/** The left rail: identity, grouped navigation, and the ingest pulse.

    The pulse is at the bottom on purpose. It is the one number that is true
    everywhere — spans are still arriving — and putting it in the frame means
    no screen has to spend space repeating it. */
export function NavRail({ screen, badges, ingest, onGo, version = '0.19' }) {
  return <aside style={{
    width: 'var(--rail-width)', flex: 'none', background: 'var(--bg-raised)',
    borderRight: '1px solid var(--hairline)', display: 'flex', flexDirection: 'column',
    position: 'sticky', top: 0, height: '100vh',
  }}>
    <div style={{
      display: 'flex', alignItems: 'center', gap: 9, padding: '14px 16px 13px',
      borderBottom: '1px solid var(--hairline)',
    }}>
      <Lockup size={19} />
      <span style={{
        marginLeft: 'auto', fontFamily: 'var(--font-mono)', fontSize: 11, color: 'var(--ink-faint)',
      }}>{version}</span>
    </div>

    <nav aria-label="Screens" style={{ flex: 1, overflowY: 'auto', padding: '10px 0' }}>
      {NAV.map((group) => <div key={group.label}>
        <div style={{
          padding: '9px 16px 5px', fontSize: 11, textTransform: 'uppercase',
          letterSpacing: 'var(--tracking-caps)', color: 'var(--ink-faint)', fontWeight: 500,
        }}>{group.label}</div>
        {group.items.map((item) => <NavItem key={item.id} item={item}
          active={screen === item.id} badge={badges?.[item.id]} onGo={onGo} />)}
      </div>)}
    </nav>

    <div style={{ borderTop: '1px solid var(--hairline)', padding: '10px 16px 12px' }}>
      <div style={{ display: 'flex', alignItems: 'baseline', justifyContent: 'space-between', marginBottom: 5 }}>
        <span style={{
          fontSize: 11, textTransform: 'uppercase', letterSpacing: 'var(--tracking-caps)',
          color: 'var(--ink-faint)', fontWeight: 500,
        }}>ingest</span>
        <span style={{
          fontFamily: 'var(--font-mono)', fontSize: 12, fontVariantNumeric: 'tabular-nums',
          color: 'var(--accent)',
        }}>{ingest?.rate == null ? '—' : fmtRate(ingest.rate)}</span>
      </div>
      <Sparkbar values={ingest?.spark || []} height={20} />
      <div style={{
        display: 'flex', alignItems: 'center', gap: 6, marginTop: 9,
        fontSize: 11, color: 'var(--ink-muted)',
      }}>
        <LiveDot color={ingest?.live ? 'var(--ok)' : 'var(--ink-faint)'} />
        <span>1 node</span>
        <span style={{ color: 'var(--ink-faint)' }}>·</span>
        <span style={{ fontFamily: 'var(--font-mono)' }}>{ingest?.durability || '—'}</span>
      </div>
    </div>
  </aside>;
}

function IconButton({ title, active, onClick, children }) {
  const [hover, setHover] = React.useState(false);
  return <div onClick={onClick} title={title} role="button" tabIndex={0} aria-pressed={active}
    onKeyDown={(e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); onClick(); } }}
    onMouseEnter={() => setHover(true)} onMouseLeave={() => setHover(false)}
    style={{
      padding: '2px 6px', borderRadius: 2, cursor: 'pointer', display: 'flex',
      background: active ? 'var(--accent-tint)' : hover ? 'var(--bg-sunken)' : 'transparent',
      color: active ? 'var(--accent-hover)' : 'var(--ink-faint)',
    }}>{children}</div>;
}

/** The sticky header: where you are, the way to anywhere, and the two
    display choices that apply to every screen. */
export function Header({ screen, subtitle, recordCount, density, onDensity, theme, onTheme, onPalette, onToken }) {
  const [title, defaultSub] = SCREENS[screen] || ['Traza', ''];
  const [hover, setHover] = React.useState(false);
  return <header style={{
    display: 'flex', alignItems: 'center', gap: 12, padding: '9px 20px',
    borderBottom: '1px solid var(--hairline)', background: 'var(--bg-raised)',
    position: 'sticky', top: 0, zIndex: 20,
  }}>
    <div style={{ minWidth: 0, display: 'flex', alignItems: 'baseline', gap: 9 }}>
      <span style={{ fontSize: 14, fontWeight: 600, color: 'var(--ink)', whiteSpace: 'nowrap' }}>{title}</span>
      <span style={{
        fontSize: 12, color: 'var(--ink-muted)', overflow: 'hidden',
        textOverflow: 'ellipsis', whiteSpace: 'nowrap',
      }}>{subtitle ?? defaultSub}</span>
    </div>

    <div onClick={onPalette} role="button" tabIndex={0} aria-label="Open the command palette"
      onKeyDown={(e) => { if (e.key === 'Enter') onPalette(); }}
      onMouseEnter={() => setHover(true)} onMouseLeave={() => setHover(false)}
      style={{
        marginLeft: 'auto', display: 'flex', alignItems: 'center', gap: 8, minWidth: 260,
        padding: '4px 8px', border: `1px solid ${hover ? 'var(--ink-faint)' : 'var(--hairline)'}`,
        borderRadius: 'var(--radius-control)', background: 'var(--bg)',
        color: 'var(--ink-faint)', cursor: 'pointer',
      }}>
      <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
        <circle cx="11" cy="11" r="7" /><path d="M20 20l-4.3-4.3" />
      </svg>
      <span style={{ fontSize: 12 }}>Jump to trace, session, view…</span>
      <Kbd style={{ marginLeft: 'auto' }}>⌘K</Kbd>
    </div>

    <div role="group" aria-label="Row density" style={{
      display: 'flex', alignItems: 'center', gap: 2, border: '1px solid var(--hairline)',
      borderRadius: 'var(--radius-control)', padding: 1,
    }}>
      <IconButton title="Comfortable rows" active={density === 'comfortable'} onClick={() => onDensity('comfortable')}>
        <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
          <path d="M4 7h16M4 12h16M4 17h16" />
        </svg>
      </IconButton>
      <IconButton title="Dense rows" active={density === 'dense'} onClick={() => onDensity('dense')}>
        <svg width="13" height="13" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
          <path d="M4 5h16M4 9h16M4 13h16M4 17h16M4 21h16" />
        </svg>
      </IconButton>
    </div>

    <IconButton title={theme === 'dark' ? 'Switch to light theme' : 'Switch to dark theme'} onClick={onTheme}>
      {theme === 'dark'
        ? <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
            <circle cx="12" cy="12" r="4" />
            <path d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4" />
          </svg>
        : <svg width="15" height="15" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5" aria-hidden="true">
            <path d="M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8z" />
          </svg>}
    </IconButton>

    <div style={{
      fontFamily: 'var(--font-mono)', fontSize: 12, fontVariantNumeric: 'tabular-nums',
      color: 'var(--ink-muted)', whiteSpace: 'nowrap',
    }}>{recordCount == null ? '' : fmtNum(recordCount) + ' records'}</div>

    <div onClick={onToken} role="button" tabIndex={0}
      onKeyDown={(e) => { if (e.key === 'Enter') onToken(); }}
      style={{
        fontSize: 12, fontWeight: 500, padding: '3px 10px', border: '1px solid var(--hairline)',
        borderRadius: 'var(--radius-control)', background: 'var(--bg-raised)',
        color: 'var(--ink)', cursor: 'pointer', whiteSpace: 'nowrap',
      }}>Set token</div>
  </header>;
}
