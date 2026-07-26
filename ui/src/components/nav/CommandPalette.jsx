import React from 'react';
import { Kbd } from '../primitives/Chrome.jsx';
import { NAV } from './Shell.jsx';

// ⌘K. The front door a trace id copied from a log never had: paste it here
// and you land on the trace, instead of searching spans and hoping it shows
// up in the first hundred rows.

/** Anything that looks like an id gets an "open it" action, without a lookup
    first. Guessing wrong costs one 404 screen with a way back; making the
    user search costs them the thing they already knew. */
function idActions(text) {
  const trimmed = text.trim();
  if (!trimmed || /\s/.test(trimmed)) return [];
  // Ids in the wild are hex, uuid, or a prefixed slug — anything without
  // whitespace that is long enough to be deliberate.
  if (trimmed.length < 6) return [];
  return [
    // `trace` singular — the detail screen is `#/trace/<id>`; `#/traces` is
    // the search screen and would silently swallow the id.
    { id: 'open-trace', label: `Open trace ${trimmed}`, hint: 'trace', go: ['trace', trimmed] },
    { id: 'open-session', label: `Open session ${trimmed}`, hint: 'session', go: ['sessions', trimmed] },
  ];
}

export function CommandPalette({ open, onClose, onNavigate, recents = [] }) {
  const [text, setText] = React.useState('');
  const [cursor, setCursor] = React.useState(0);
  const inputRef = React.useRef(null);
  const restoreTo = React.useRef(null);

  React.useEffect(() => {
    if (!open) return undefined;
    // Remember what had focus so closing puts it back — a palette that
    // dumps focus on the body makes the next Tab start from the top of the
    // page, which is disorienting every single time.
    restoreTo.current = document.activeElement;
    setText('');
    setCursor(0);
    const timer = setTimeout(() => inputRef.current?.focus(), 0);
    return () => {
      clearTimeout(timer);
      if (restoreTo.current instanceof HTMLElement) restoreTo.current.focus();
    };
  }, [open]);

  const screens = React.useMemo(
    () => NAV.flatMap((group) => group.items.map((item) => ({
      id: 'go-' + item.id, label: item.label, hint: group.label, go: [item.id],
    }))),
    [],
  );

  const results = React.useMemo(() => {
    const query = text.trim().toLowerCase();
    const matches = query
      ? screens.filter((entry) => entry.label.toLowerCase().includes(query)
        || entry.hint.toLowerCase().includes(query))
      : screens;
    return [...idActions(text), ...matches, ...(query ? [] : recents)].slice(0, 12);
  }, [text, screens, recents]);

  React.useEffect(() => { setCursor(0); }, [text]);

  if (!open) return null;

  const choose = (entry) => {
    if (!entry) return;
    onNavigate(entry.go);
    onClose();
  };

  const onKeyDown = (event) => {
    if (event.key === 'Escape') { event.preventDefault(); onClose(); return; }
    if (event.key === 'ArrowDown' || (event.key === 'n' && event.ctrlKey)) {
      event.preventDefault();
      setCursor((at) => Math.min(results.length - 1, at + 1));
    } else if (event.key === 'ArrowUp' || (event.key === 'p' && event.ctrlKey)) {
      event.preventDefault();
      setCursor((at) => Math.max(0, at - 1));
    } else if (event.key === 'Enter') {
      event.preventDefault();
      choose(results[cursor]);
    }
  };

  return <div onClick={onClose} style={{
    position: 'fixed', inset: 0, background: 'rgba(31,27,23,0.4)', zIndex: 80,
    display: 'flex', alignItems: 'flex-start', justifyContent: 'center', paddingTop: '12vh',
  }}>
    <div role="dialog" aria-modal="true" aria-label="Command palette"
      onClick={(event) => event.stopPropagation()}
      style={{
        width: 560, maxWidth: '92vw', background: 'var(--bg-raised)',
        border: '1px solid var(--hairline)', borderRadius: 'var(--radius-card)',
        overflow: 'hidden', fontFamily: 'var(--font-sans)',
      }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 9, padding: '10px 14px', borderBottom: '1px solid var(--hairline)' }}>
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="var(--ink-faint)" strokeWidth="1.5" aria-hidden="true">
          <circle cx="11" cy="11" r="7" /><path d="M20 20l-4.3-4.3" />
        </svg>
        <input ref={inputRef} value={text} onChange={(event) => setText(event.target.value)}
          onKeyDown={onKeyDown} placeholder="Jump to trace, session, view…"
          aria-label="Search screens, traces and sessions"
          style={{
            flex: 1, border: 'none', outline: 'none', background: 'transparent',
            font: 'inherit', fontSize: 13, color: 'var(--ink)',
          }} />
        <Kbd>esc</Kbd>
      </div>
      <div role="listbox" style={{ maxHeight: '46vh', overflowY: 'auto', padding: '4px 0' }}>
        {results.length === 0
          ? <div style={{ padding: '14px', fontSize: 13, color: 'var(--ink-muted)' }}>Nothing matches.</div>
          : results.map((entry, index) => <div key={entry.id} role="option" aria-selected={index === cursor}
            onMouseEnter={() => setCursor(index)} onClick={() => choose(entry)}
            style={{
              display: 'flex', alignItems: 'center', gap: 10, padding: '6px 14px', cursor: 'pointer',
              background: index === cursor ? 'var(--bg-sunken)' : 'transparent',
              borderLeft: `2px solid ${index === cursor ? 'var(--accent)' : 'transparent'}`,
            }}>
            <span style={{
              flex: 1, minWidth: 0, fontSize: 13, color: 'var(--ink)',
              overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap',
            }}>{entry.label}</span>
            <span style={{
              fontSize: 11, textTransform: 'uppercase', letterSpacing: 'var(--tracking-caps)',
              color: 'var(--ink-faint)',
            }}>{entry.hint}</span>
          </div>)}
      </div>
      <div style={{
        display: 'flex', gap: 12, padding: '8px 14px', borderTop: '1px solid var(--hairline)',
        fontSize: 11, color: 'var(--ink-faint)',
      }}>
        <span><Kbd>↑</Kbd> <Kbd>↓</Kbd> move</span>
        <span><Kbd>↵</Kbd> open</span>
        <span style={{ marginLeft: 'auto' }}>paste a trace or session id to jump straight to it</span>
      </div>
    </div>
  </div>;
}
