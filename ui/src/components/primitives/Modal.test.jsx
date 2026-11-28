// @vitest-environment jsdom
//
// The dialog behaves like one: Escape closes it, Tab stays inside it, and
// closing returns focus to whatever opened it.

import React from 'react';
import { describe, it, expect, vi, afterEach } from 'vitest';
import { render, act, cleanup, fireEvent, screen } from '@testing-library/react';
import { Modal } from './Modal.jsx';

afterEach(cleanup);

function Harness({ onClose }) {
  const [open, setOpen] = React.useState(false);
  return <>
    <button onClick={() => setOpen(true)}>open</button>
    <Modal open={open} title="Dialog" onClose={() => { onClose(); setOpen(false); }}
      footer={<button>last</button>}>
      <input aria-label="first field" />
    </Modal>
  </>;
}

async function flush() {
  await act(async () => { await new Promise((r) => setTimeout(r, 5)); });
}

describe('Modal', () => {
  it('closes on Escape and gives focus back to the opener', async () => {
    const onClose = vi.fn();
    render(<Harness onClose={onClose} />);
    const opener = screen.getByText('open');
    opener.focus();
    fireEvent.click(opener);
    await flush();
    expect(document.activeElement).toBe(screen.getByLabelText('first field'));
    fireEvent.keyDown(document.activeElement, { key: 'Escape' });
    await flush();
    expect(onClose).toHaveBeenCalledTimes(1);
    expect(screen.queryByRole('dialog')).toBeNull();
    expect(document.activeElement).toBe(opener);
  });

  it('keeps Tab inside the dialog, in both directions', async () => {
    render(<Harness onClose={() => {}} />);
    fireEvent.click(screen.getByText('open'));
    await flush();
    const last = screen.getByText('last');
    last.focus();
    fireEvent.keyDown(last, { key: 'Tab' });
    expect(document.activeElement).toBe(screen.getByLabelText('Close'));
    fireEvent.keyDown(document.activeElement, { key: 'Tab', shiftKey: true });
    expect(document.activeElement).toBe(last);
  });

  it('is a labelled modal dialog', async () => {
    render(<Harness onClose={() => {}} />);
    fireEvent.click(screen.getByText('open'));
    await flush();
    const dialog = screen.getByRole('dialog', { name: 'Dialog' });
    expect(dialog.getAttribute('aria-modal')).toBe('true');
  });
});
