import React from 'react';

// The one dialog. It behaves like one: Escape closes it, Tab cannot wander
// out of it to the page behind, and closing puts focus back where it was —
// a dialog that drops focus on <body> makes the next Tab start from the top
// of the page, which is disorienting every single time.

const FOCUSABLE = 'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), '
  + 'textarea:not([disabled]), [tabindex]:not([tabindex="-1"])';

let nextId = 0;

export function Modal({ open, title, children, footer, onClose, width = 480 }) {
  const dialogRef = React.useRef(null);
  const closeRef = React.useRef(onClose);
  closeRef.current = onClose;
  const [titleId] = React.useState(() => 'tz-modal-' + (nextId += 1));

  React.useEffect(() => {
    if (!open) return undefined;
    const restoreTo = document.activeElement;
    // First focusable thing that is not the close button, so a prompt opens
    // with its input focused; else the dialog itself.
    const timer = setTimeout(() => {
      const dialog = dialogRef.current;
      if (!dialog) return;
      const items = Array.from(dialog.querySelectorAll(FOCUSABLE)).filter((el) => !el.dataset.modalClose);
      (items[0] || dialog).focus();
    }, 0);

    const onKey = (event) => {
      if (event.key === 'Escape') {
        event.preventDefault();
        event.stopPropagation();
        closeRef.current?.();
        return;
      }
      if (event.key !== 'Tab') return;
      const dialog = dialogRef.current;
      if (!dialog) return;
      const items = Array.from(dialog.querySelectorAll(FOCUSABLE));
      if (!items.length) { event.preventDefault(); dialog.focus(); return; }
      const first = items[0];
      const last = items[items.length - 1];
      const active = document.activeElement;
      if (event.shiftKey && (active === first || !dialog.contains(active))) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && (active === last || !dialog.contains(active))) {
        event.preventDefault();
        first.focus();
      }
    };
    // Capture phase, so a screen's own global shortcuts (Escape resets a
    // waterfall zoom) do not also fire underneath an open dialog.
    document.addEventListener('keydown', onKey, true);
    return () => {
      clearTimeout(timer);
      document.removeEventListener('keydown', onKey, true);
      if (restoreTo instanceof HTMLElement && document.contains(restoreTo)) restoreTo.focus();
    };
  }, [open]);

  if (!open) return null;
  return <div onClick={onClose} style={{ position: 'fixed', inset: 0, background: 'rgba(31,27,23,0.4)', display: 'flex', alignItems: 'center', justifyContent: 'center', zIndex: 90, padding: 12 }}>
    <div ref={dialogRef} role="dialog" aria-modal="true" aria-labelledby={titleId} tabIndex={-1}
      onClick={(e) => e.stopPropagation()}
      style={{ background: 'var(--bg-raised)', border: '1px solid var(--hairline)', borderRadius: 'var(--radius-card)', width, maxWidth: '100%', maxHeight: '88vh', display: 'flex', flexDirection: 'column', fontFamily: 'var(--font-sans)', outline: 'none' }}>
      <div style={{ display: 'flex', alignItems: 'center', justifyContent: 'space-between', padding: '12px 16px', borderBottom: '1px solid var(--hairline)' }}>
        <span id={titleId} style={{ fontSize: 'var(--text-14)', lineHeight: 'var(--lh-14)', fontWeight: 600, color: 'var(--ink)' }}>{title}</span>
        <button type="button" data-modal-close="true" aria-label="Close" onClick={onClose}
          style={{ border: 'none', background: 'transparent', color: 'var(--ink-faint)', cursor: 'pointer', fontSize: 16, lineHeight: 1, padding: 0 }}>×</button>
      </div>
      <div style={{ padding: '16px', overflow: 'auto', color: 'var(--ink)', fontSize: 'var(--text-13)', lineHeight: 'var(--lh-13)' }}>{children}</div>
      {footer ? <div style={{ display: 'flex', justifyContent: 'flex-end', flexWrap: 'wrap', gap: '8px', padding: '12px 16px', borderTop: '1px solid var(--hairline)' }}>{footer}</div> : null}
    </div>
  </div>;
}
