// @vitest-environment jsdom
//
// Mission Control against a fake server at the network boundary: `fetch` is
// replaced, the API client is real. The stream is the bytes a 0.28.0 server
// sent (`live_stream.txt`), held open the way the server holds it; every
// other answer is a captured response (`fuse_run_held.json` is the verdict
// of a run a min_success_odds policy held, `nudges.json` a delivered and an
// expired nudge, `nudge_created.json` a POST's 201) with the run id it was
// asked for written in.

import React from 'react';
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, act, cleanup, fireEvent, screen, within } from '@testing-library/react';
import streamText from '../test/fixtures/live_stream.txt?raw';
import listing from '../test/fixtures/live_runs.json';
import heldVerdict from '../test/fixtures/fuse_run_held.json';
import tripped from '../test/fixtures/fuse_trip.json';
import resetDoc from '../test/fixtures/reset_grant.json';
import nudgesDoc from '../test/fixtures/nudges.json';
import nudgeCreated from '../test/fixtures/nudge_created.json';
import fateRun from '../test/fixtures/fate_run_held.json';
import { setToken } from '../lib/api.js';
import { setIdentity } from '../lib/session.js';
import { LiveScreen } from './LiveScreen.jsx';

const CAPTURE_NS = Math.max(...listing.runs.map((row) => row.last_activity_ns));
const json = (status, body) => new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });

/** A held-open SSE response: `push(text)` sends more, `end()` closes it. */
function sse() {
  let controller;
  const body = new ReadableStream({ start(c) { controller = c; } });
  const encoder = new TextEncoder();
  return {
    response: new Response(body, { status: 200, headers: { 'Content-Type': 'text/event-stream' } }),
    push: (text) => controller.enqueue(encoder.encode(text)),
    end: () => controller.close(),
  };
}

/** The fake server. `streams` collects every stream opened; `calls` every request. */
function serve({ streamStatus = 200, streamError } = {}) {
  const server = { calls: [], streams: [], nudges: [], heldRuns: new Set(['tkt-4162']) };
  globalThis.fetch = vi.fn(async (url, init = {}) => {
    const href = String(url);
    const [path, query = ''] = href.split('?');
    const method = init.method || 'GET';
    const call = { href, path, query, method, headers: init.headers || {}, body: init.body ? JSON.parse(init.body) : undefined };
    server.calls.push(call);
    if (path === '/v1/runs/live/stream') {
      if (streamStatus !== 200) return json(streamStatus, { error: streamError });
      const stream = sse();
      server.streams.push(stream);
      stream.push(streamText);
      return stream.response;
    }
    if (path === '/v1/runs/live') return json(200, listing);
    let match = /^\/v1\/fuses\/runs\/([^/]+)$/.exec(path);
    if (match) {
      const run = decodeURIComponent(match[1]);
      if (server.heldRuns.has(run)) return json(200, { ...heldVerdict, run });
      return json(200, { ...resetDoc, run, verdict: 'continue', grant: null, foresight: undefined });
    }
    match = /^\/v1\/fuses\/runs\/([^/]+)\/(trip|reset)$/.exec(path);
    if (match) return json(200, match[2] === 'trip' ? { ...tripped, run: decodeURIComponent(match[1]) } : { ...resetDoc, run: decodeURIComponent(match[1]) });
    match = /^\/v1\/runs\/([^/]+)\/nudges$/.exec(path);
    if (match) {
      const run = decodeURIComponent(match[1]);
      if (method === 'POST') {
        const doc = { ...nudgeCreated, run, message: call.body.message, kind: call.body.kind };
        server.nudges.push(doc);
        return json(201, doc);
      }
      return json(200, { nudges: server.nudges.filter((n) => n.run === run), run, tenant: '' });
    }
    if (/^\/v1\/fate\/runs\//.test(path)) return json(200, fateRun);
    return json(404, { error: 'not found' });
  });
  return server;
}

const RW = { auth: 'token', scope: 'rw', operator: true, tenant: null };
const RO = { auth: 'token', scope: 'ro', operator: false, tenant: null };

async function settle(ms = 0) {
  await act(async () => { await vi.advanceTimersByTimeAsync(ms); });
}

async function show(identity = RW, params = new URLSearchParams()) {
  const pushToast = vi.fn();
  render(<LiveScreen go={vi.fn()} params={params} identity={identity} pushToast={pushToast} />);
  await settle(10);
  await settle(10);
  return { pushToast };
}

const rows = () => screen.queryAllByTestId('live-row');
const row = (run) => rows().find((node) => node.dataset.run === run);

beforeEach(() => {
  vi.useFakeTimers({ toFake: ['setTimeout', 'setInterval', 'clearTimeout', 'clearInterval', 'Date'] });
  vi.setSystemTime(new Date(CAPTURE_NS / 1e6 + 2000));
  Object.defineProperty(document, 'hidden', { configurable: true, get: () => false });
  window.matchMedia = undefined;
  setToken('');
  setIdentity(null);
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

describe('the live view', () => {
  it('streams the runs in over one held connection, the token in a header and never in the URL', async () => {
    setToken('trz_secret_token');
    const server = serve();
    await show();
    expect(rows()).toHaveLength(listing.runs.length);
    const stream = server.calls.find((c) => c.path === '/v1/runs/live/stream');
    expect(stream.headers.Authorization).toBe('Bearer trz_secret_token');
    expect(server.calls.every((c) => !c.href.includes('trz_secret'))).toBe(true);
    expect(screen.getByTestId('live-status').dataset.state).toBe('live');
  });

  it('puts what needs a person first: stopped runs, then likely failures', async () => {
    serve();
    await show();
    const guards = rows().map((node) => node.dataset.guard);
    expect(guards.slice(0, 4)).toEqual(['stop', 'stop', 'stop', 'stop']);
    const order = rows().map((node) => node.dataset.run);
    expect(order.indexOf('tkt-4162')).toBeLessThan(order.indexOf('tkt-4173'));
    // The finished run is quiet: last.
    expect(order[order.length - 1]).toBe('tkt-4142');
  });

  it('shows each run\'s step, spend, guard verdict, foresight against its base rate, and nudges waiting', async () => {
    serve();
    await show();
    const held = row('tkt-4162').textContent;
    expect(held).toContain('search_orders');
    expect(held).toContain('likely failure');
    expect(held).toContain('10%');
    expect(held).toContain('4.7–20%');
    expect(held).toContain('1 waiting');
    expect(held).toContain('0.0016');
    const stopped = row('conv-82febb');
    expect(stopped.textContent).toContain('next model call');
    expect(stopped.textContent).toContain('no history');
    expect(within(stopped).getByTitle(/max_handoff_cycles 3 reached/)).toBeTruthy();
    expect(row('conv-c36f2e').textContent).toContain('novel path');
  });

  it('updates a row in place when its run changes', async () => {
    const server = serve();
    await show();
    const before = listing.runs.find((r) => r.run === 'tkt-4173');
    const changed = { ...before, cost_usd: 0.0421, steps: before.steps + 1, last_activity_ns: before.last_activity_ns + 1e9 };
    await act(async () => { server.streams[0].push(`event: run\ndata: ${JSON.stringify(changed)}\n\n`); });
    await settle(10);
    expect(row('tkt-4173').textContent).toContain('0.0421');
    expect(rows()).toHaveLength(listing.runs.length);
  });

  it('finds a run a predictive limit holds, which the listing cannot say, and offers to approve it', async () => {
    const server = serve();
    await show();
    expect(row('tkt-4162').dataset.guard).toBe('continue');
    await settle(3100);
    await settle(10);
    expect(server.calls.some((c) => c.path === '/v1/fuses/runs/tkt-4162' && c.query === 'tenant=')).toBe(true);
    expect(row('tkt-4162').dataset.guard).toBe('held');
    fireEvent.click(within(row('tkt-4162')).getByText('Approve'));
    const dialog = screen.getByRole('dialog', { name: 'Approve to continue' });
    expect(dialog.textContent).toMatch(/foresight stop is a prediction/);
    fireEvent.change(within(dialog).getByLabelText('min_success_odds'), { target: { value: '0.05' } });
    await act(async () => { fireEvent.click(within(dialog).getByText('Reset the run')); });
    await settle(10);
    const reset = server.calls.find((c) => c.path === '/v1/fuses/runs/tkt-4162/reset');
    expect(reset.body).toEqual({ grant: { min_success_odds: 0.05 } });
    expect(reset.query).toBe('tenant=');
  });

  it('sends a nudge and follows it to delivery', async () => {
    const server = serve();
    await show();
    fireEvent.click(within(row('tkt-4173')).getByText('Nudge'));
    const dialog = screen.getByRole('dialog', { name: 'Nudge tkt-4173' });
    await settle(10);
    // What runs like this one did next, from the run route, as context.
    expect(dialog.textContent).toContain('runs that took this path succeeded 6 of 60 times');
    fireEvent.click(within(dialog).getByText('Send nudge'));
    expect(within(dialog).getByRole('alert').textContent).toMatch(/Write the message/);
    fireEvent.change(within(dialog).getByLabelText('Nudge message'), { target: { value: 'Stop searching; issue the refund.' } });
    fireEvent.click(within(dialog).getByText('10 min'));
    await act(async () => { fireEvent.click(within(dialog).getByText('Send nudge')); });
    await settle(10);
    const post = server.calls.find((c) => c.method === 'POST' && c.path === '/v1/runs/tkt-4173/nudges');
    expect(post.body).toEqual({ message: 'Stop searching; issue the refund.', kind: 'advice', ttl_seconds: 600 });
    expect(post.query).toBe('tenant=');
    expect(within(dialog).getByTestId('nudge-delivery').textContent).toMatch(/pending.*waiting for the run's next pre-call check/);

    // The run checks; the server marks it delivered. The dialog polls.
    const delivered = nudgesDoc.nudges.find((n) => n.status === 'delivered');
    server.nudges[0] = { ...server.nudges[0], status: 'delivered', delivered_at_ns: server.nudges[0].created_at_ns + (delivered.delivered_at_ns - delivered.created_at_ns) };
    await settle(1600);
    await settle(10);
    expect(within(dialog).getByTestId('nudge-delivery').textContent).toMatch(/delivered.*after 2\.67 s/);
  });

  it('sends a veto as a block nudge', async () => {
    const server = serve();
    await show();
    fireEvent.click(within(row('tkt-4173')).getByText('Nudge'));
    const dialog = screen.getByRole('dialog', { name: 'Nudge tkt-4173' });
    fireEvent.click(within(dialog).getByText('block'));
    expect(dialog.textContent).toMatch(/A veto: the check that delivers it answers stop/);
    fireEvent.change(within(dialog).getByLabelText('Nudge message'), { target: { value: 'Hold refunds over $500 for a human.' } });
    await act(async () => { fireEvent.click(within(dialog).getByText('Send veto')); });
    expect(server.calls.find((c) => c.method === 'POST').body.kind).toBe('block');
  });

  it('pulls the kill switch on a running run', async () => {
    const server = serve();
    const { pushToast } = await show();
    fireEvent.click(within(row('tkt-4173')).getByText('Kill'));
    const dialog = screen.getByRole('dialog', { name: 'Kill switch' });
    fireEvent.change(within(dialog).getByPlaceholderText(/customer escalated/), { target: { value: 'taking over by hand' } });
    await act(async () => { fireEvent.click(within(dialog).getByText('Stop this run')); });
    await settle(10);
    const trip = server.calls.find((c) => c.path === '/v1/fuses/runs/tkt-4173/trip');
    expect(trip.body).toEqual({ reason: 'taking over by hand' });
    expect(pushToast).toHaveBeenCalledWith(expect.objectContaining({ title: 'Kill switch on' }));
  });

  it('offers a read-only credential no controls, and never asks it to write', async () => {
    const server = serve();
    await show(RO);
    expect(screen.queryAllByText('Nudge')).toHaveLength(0);
    expect(screen.queryAllByText('Kill')).toHaveLength(0);
    expect(screen.queryAllByText('Approve')).toHaveLength(0);
    expect(screen.getAllByText('Tape').length).toBe(listing.runs.length);
    expect(server.calls.filter((c) => c.method === 'POST')).toEqual([]);
  });

  it('filters by state, and the summary\'s counts are the filters', async () => {
    serve();
    await show(RW, new URLSearchParams('filter=nudged'));
    expect(rows().map((node) => node.dataset.run)).toEqual(['tkt-4162']);
    cleanup();
    serve();
    await show(RW, new URLSearchParams('filter=stopped'));
    expect(rows().every((node) => node.dataset.guard === 'stop')).toBe(true);
  });

  it('lays the runs out as cards on a phone', async () => {
    window.matchMedia = (query) => ({ matches: query.includes('max-width: 699px'), media: query, addEventListener() {}, removeEventListener() {} });
    serve();
    await show();
    expect(rows()).toHaveLength(0);
    const cards = screen.getAllByTestId('live-card');
    expect(cards).toHaveLength(listing.runs.length);
    expect(cards.find((c) => c.dataset.run === 'tkt-4162').textContent).toMatch(/likely failure[\s\S]*search_orders/);
  });
});

describe('when the stream is not there', () => {
  it('polls the listing when the stream is refused for want of a slot, and says so', async () => {
    const server = serve({ streamStatus: 409, streamError: 'the every-tenant view already has 8 live-run streams open; close one, or poll GET /v1/runs/live' });
    await show();
    expect(rows()).toHaveLength(listing.runs.length);
    expect(screen.getByTestId('live-status').dataset.state).toBe('polling');
    expect(document.body.textContent).toMatch(/The stream was refused/);
    const polls = () => server.calls.filter((c) => c.path === '/v1/runs/live').length;
    const first = polls();
    expect(server.calls.find((c) => c.path === '/v1/runs/live').query).toBe('limit=256');
    await settle(5000);
    expect(polls()).toBe(first + 1);
  });

  it('says plainly when the store runs without fuses', async () => {
    serve({ streamStatus: 409, streamError: 'live runs and nudges need run fuses, which are disabled on this store (traza-server --no-fuses)' });
    await show();
    expect(document.body.textContent).toMatch(/started with --no-fuses/);
    expect(rows()).toHaveLength(0);
  });

  it('reconnects after a break and rebuilds the view from a fresh snapshot', async () => {
    const server = serve();
    await show();
    await act(async () => { server.streams[0].end(); });
    await settle(10);
    expect(screen.getByTestId('live-status').dataset.state).toBe('reconnecting');
    expect(screen.getByTestId('live-status').textContent).toMatch(/rows as of/);
    expect(rows()).toHaveLength(listing.runs.length);
    await settle(600);
    await settle(10);
    expect(server.streams).toHaveLength(2);
    expect(screen.getByTestId('live-status').dataset.state).toBe('live');
    expect(document.body.textContent).toMatch(/rebuilt from a fresh snapshot/);
  });
});
