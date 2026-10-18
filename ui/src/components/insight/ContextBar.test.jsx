// @vitest-environment jsdom
//
// The context bar over the chunks a 0.28.0 server's X-Ray returned for step
// 30 of fix-parser-7 (`xray_fix7_30.json`) — and over 2,000 of them, the most
// one X-Ray carries, made by repeating those 38 under fresh indices.

import React from 'react';
import { describe, it, expect, afterEach } from 'vitest';
import { render, cleanup, screen, within } from '@testing-library/react';
import xray from '../../test/fixtures/insight/xray_fix7_30.json';
import { ContextBar } from './ContextBar.jsx';

afterEach(cleanup);

const many = Array.from({ length: 2000 }, (_, i) => ({ ...xray.chunks[i % xray.chunks.length], index: i }));

describe('the context bar', () => {
  it('lays every chunk of a 2,000-chunk context on a canvas that scrolls, rather than cutting the last ones off', () => {
    render(<ContextBar chunks={many} />);
    expect(screen.getAllByTestId('context-segment')).toHaveLength(2000);
    // Each chunk keeps two pixels and a one-pixel gap: the canvas is at
    // least that wide, whatever the card's width...
    const canvas = screen.getByTestId('context-canvas');
    expect(parseFloat(canvas.style.minWidth)).toBeGreaterThanOrEqual(2000 * 3 - 1);
    // ...and sits in a box that scrolls sideways instead of hiding the rest.
    expect(canvas.parentElement.style.overflowX).toBe('auto');
    // "last read" marks the canvas's own end, not the edge of the view.
    expect(within(canvas).getByText('last read')).toBeTruthy();
  });

  it('needs no more room than the card for a short context', () => {
    render(<ContextBar chunks={xray.chunks} />);
    const canvas = screen.getByTestId('context-canvas');
    expect(parseFloat(canvas.style.minWidth)).toBe(xray.chunks.length * 3 - 1);
    expect(canvas.style.width).toBe('100%');
  });
});
