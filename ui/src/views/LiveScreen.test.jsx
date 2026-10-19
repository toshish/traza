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
import badTenant from '../test/fixtures/live_bad_tenant.json';
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
    // A test may hold a request until it says so.
    const gate = server.gate && server.gate(call);
    if (gate) await gate;
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
      if (server.failReads > 0) { server.failReads -= 1; return json(503, { error: 'the store could not serve this' }); }
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
  const go = vi.fn();
  render(<LiveScreen go={go} params={params} identity={identity} pushToast={pushToast} />);
  await settle(10);
  await settle(10);
  return { pushToast, go };
}

/** A promise and the function that settles it. */
function deferred() {
  let release;
  const promise = new Promise((done) => { release = done; });
  return { promise, release };
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

  it('drops a run the server says left the view, and says when foresight was deferred', async () => {
    const server = serve();
    await show();
    expect(row('tkt-4173')).toBeTruthy();
    // The guard let go of one run; another's foresight read was not made.
    const deferredRow = { ...listing.runs.find((r) => r.run === 'tkt-4162'), foresight: undefined, foresight_deferred: true };
    server.streams[0].push('event: removed\ndata: {"reason":"evicted","run":"tkt-4173","tenant":""}\n\n'
      + 'event: run\ndata: ' + JSON.stringify(deferredRow) + '\n\n');
    await settle(10);
    expect(row('tkt-4173')).toBeUndefined();
    expect(rows()).toHaveLength(listing.runs.length - 1);
    expect(within(row('tkt-4162')).getByText('foresight deferred')).toBeTruthy();
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
    // Asked about search_orders a minute ago, no span since: not "continue".
    expect(row('tkt-4162').dataset.guard).toBe('waiting');
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


describe('review follow-ups', () => {
  it('calls a long-pending run waiting, not continue, and says a hold on the step alone cannot be read from here', async () => {
    serve();
    await show();
    const waiting = row('tkt-4174');
    expect(waiting.dataset.guard).toBe('continue');
    await settle(10_000);
    // The probe found no stop for the path so far: the run asked, and has
    // not taken its step.
    expect(row('tkt-4174').dataset.guard).toBe('waiting');
    fireEvent.click(row('tkt-4174').querySelector('[role="row"]'));
    expect(screen.getByTestId('live-detail').textContent).toMatch(/cannot be read from here/);
  });

  it('marks held only for the path so far, and says so', async () => {
    serve();
    await show();
    await settle(3100);
    await settle(10);
    fireEvent.click(row('tkt-4162').querySelector('[role="row"]'));
    const detail = screen.getByTestId('live-detail').textContent;
    expect(detail).toMatch(/the path so far/);
    expect(detail).toMatch(/cannot be read from here/);
  });

  it('says the server refused the view, in its words, instead of claiming no run is live', async () => {
    const server = serve({ streamStatus: 400, streamError: badTenant.error });
    await show(RW, new URLSearchParams('tenant=Acme Corp'));
    await settle(20_000);
    expect(document.body.textContent).toContain('tenant must be lowercase');
    expect(document.body.textContent).not.toMatch(/No run has had activity/);
    expect(screen.getByTestId('live-status').dataset.state).toBe('rejected');
    // Refused, not broken: asked once, and the field to fix it is still there.
    expect(server.calls.filter((c) => c.path === '/v1/runs/live/stream')).toHaveLength(1);
    expect(screen.getByLabelText('Filter by tenant')).toBeTruthy();
  });

  it('says it is not connected yet, and why, while a server that never answered is retried', async () => {
    serve({ streamStatus: 503, streamError: 'the store is warming up' });
    await show();
    await settle(20_000);
    expect(document.body.textContent).not.toMatch(/No run has had activity/);
    expect(document.body.textContent).toMatch(/Not connected yet/);
    expect(document.body.textContent).toContain('the store is warming up');
  });

  it('takes a stream that goes silent without closing as dead, and reconnects from a fresh snapshot', async () => {
    const server = serve();
    await show();
    await settle(39_000);
    expect(server.streams).toHaveLength(1);
    expect(screen.getByTestId('live-status').dataset.state).toBe('live');
    await settle(1_500);
    await settle(600);
    await settle(10);
    expect(server.streams).toHaveLength(2);
    expect(document.body.textContent).toMatch(/rebuilt from a fresh snapshot/);
  });

  it('shows how many runs stand behind a rate, and marks a novel run\'s rate as the one before it left the path', async () => {
    serve();
    await show();
    expect(row('tkt-4162').textContent).toMatch(/n\s*60/);
    const novel = row('conv-c36f2e').textContent;
    expect(novel).toMatch(/before\s*0%/);
    cleanup();
    window.matchMedia = (query) => ({ matches: query.includes('max-width: 699px'), media: query, addEventListener() {}, removeEventListener() {} });
    serve();
    await show();
    const card = screen.getAllByTestId('live-card').find((c) => c.dataset.run === 'tkt-4162');
    expect(card.textContent).toMatch(/n\s*60/);
  });

  it('keeps following a nudge through a failed read, and sends it once however fast the keys are pressed', async () => {
    const server = serve();
    await show();
    fireEvent.click(within(row('tkt-4173')).getByText('Nudge'));
    const dialog = screen.getByRole('dialog', { name: 'Nudge tkt-4173' });
    const box = within(dialog).getByLabelText('Nudge message');
    fireEvent.change(box, { target: { value: 'Stop searching; issue the refund.' } });
    const post = deferred();
    server.gate = (c) => (c.method === 'POST' ? post.promise : null);
    await act(async () => {
      fireEvent.keyDown(box, { key: 'Enter', metaKey: true });
      fireEvent.keyDown(box, { key: 'Enter', metaKey: true });
    });
    server.gate = null;
    await act(async () => { post.release(); });
    await settle(10);
    expect(server.calls.filter((c) => c.method === 'POST')).toHaveLength(1);
    expect(within(dialog).getByTestId('nudge-delivery').textContent).toMatch(/pending\s*waiting for the run's next pre-call check/);
    // One read fails while the dialog follows the nudge…
    server.failReads = 1;
    await settle(1600);
    await settle(10);
    // …and the run's check delivers it meanwhile.
    server.nudges[0] = { ...server.nudges[0], status: 'delivered', delivered_at_ns: server.nudges[0].created_at_ns + 2_670_000_000 };
    await settle(1600);
    await settle(10);
    expect(within(dialog).getByTestId('nudge-delivery').textContent).toMatch(/delivered\s*after 2\.67 s/);
  });

  it('clears the held mark once Approve has reset the run', async () => {
    const server = serve();
    await show();
    await settle(3100);
    await settle(10);
    expect(row('tkt-4162').dataset.guard).toBe('held');
    // The grant of lower odds lifts the foresight stop.
    server.heldRuns.delete('tkt-4162');
    fireEvent.click(within(row('tkt-4162')).getByText('Approve'));
    const dialog = screen.getByRole('dialog', { name: 'Approve to continue' });
    fireEvent.change(within(dialog).getByLabelText('min_success_odds'), { target: { value: '0.05' } });
    await act(async () => { fireEvent.click(within(dialog).getByText('Reset the run')); });
    await settle(10);
    expect(row('tkt-4162').dataset.guard).not.toBe('held');
  });

  it('opens Approve on the run last asked about, whatever order the verdicts land in', async () => {
    const server = serve();
    await show();
    const first = deferred();
    const second = deferred();
    server.gate = (c) => (c.path === '/v1/fuses/runs/conv-82febb' ? first.promise
      : c.path === '/v1/fuses/runs/conv-872843' ? second.promise : null);
    fireEvent.click(within(row('conv-82febb')).getByText('Approve'));
    fireEvent.click(within(row('conv-872843')).getByText('Approve'));
    await act(async () => { second.release(); });
    await settle(10);
    await act(async () => { first.release(); });
    await settle(10);
    const dialog = screen.getByRole('dialog', { name: 'Approve to continue' });
    expect(dialog.textContent).toContain('conv-872843');
    expect(dialog.textContent).not.toContain('conv-82febb');
  });

  it('counts a recorded step\'s age from its span, not as a step still running', async () => {
    serve();
    await show();
    // tkt-4173's gpt-4o call ended when its span arrived — with the stream's
    // last frame for it — and started seconds before that.
    expect(row('tkt-4173').textContent).toMatch(/0 s ago/);
    await settle(3000);
    const cell = row('tkt-4173').textContent;
    expect(cell).toMatch(/3 s ago/);
    // Not a running time counted from the call's start.
    const started = listing.runs.find((r) => r.run === 'tkt-4173').current.started_ns;
    expect(cell).not.toMatch(/\b(6|7|8|9) s\b/);
    expect(started).toBeLessThan(CAPTURE_NS - 3e9);
  });

  it('follows the run link from the keyboard, and the link carries the run\'s tenant', async () => {
    serve();
    await show();
    const link = within(row('tkt-4173')).getByRole('link', { name: 'tkt-4173' });
    expect(link.getAttribute('href')).toBe('#/run/tkt-4173?tenant=');
    // Enter on the link is the link's: not cancelled, and the row stays shut.
    expect(fireEvent.keyDown(link, { key: 'Enter' })).toBe(true);
    expect(row('tkt-4173').querySelector('[role="row"]').getAttribute('aria-expanded')).toBe('false');
  });
});

describe('astra review fixes', () => {
  const verdictReads = (server, run) => server.calls.filter((c) => c.path === `/v1/fuses/runs/${run}` && c.method === 'GET');

  it('marks the summary partial once the page has let runs go, and never states a short count as the whole', async () => {
    const server = serve();
    // Only the run with a nudge waiting is drawn: the summary is the point.
    await show(RW, new URLSearchParams('filter=nudged'));
    expect(screen.queryByTestId('live-partial')).toBeNull();
    const template = listing.runs.find((r) => r.run === 'tkt-4173');
    let frames = '';
    for (let i = 0; i < 1001; i += 1) {
      frames += `event: run\ndata: ${JSON.stringify({ ...template, run: `bulk-${i}`, last_activity_ns: CAPTURE_NS + (i + 1) * 1e6 })}\n\n`;
    }
    await act(async () => { server.streams[0].push(frames); });
    await settle(10);
    // 1,001 runs streamed in beside the snapshot's: the page holds 1,000.
    expect(screen.getByText('1,000+')).toBeTruthy();
    expect(screen.getByTestId('live-partial').textContent).toMatch(/1,000/);
  });

  it('keeps at most four verdict reads in flight, however many runs wait and however long the reads take', async () => {
    const server = serve();
    const never = deferred();
    server.gate = (c) => (/^\/v1\/fuses\/runs\/[^/]+$/.test(c.path) ? never.promise : null);
    await show();
    // Twelve more runs that asked about a step a minute ago and took none.
    const asked = listing.runs.find((r) => r.run === 'tkt-4162');
    let frames = '';
    for (let i = 0; i < 12; i += 1) frames += `event: run\ndata: ${JSON.stringify({ ...asked, run: `wait-${i}` })}\n\n`;
    await act(async () => { server.streams[0].push(frames); });
    try {
      for (let i = 0; i < 14; i += 1) await settle(3000);
      const reads = new Set(server.calls.filter((c) => /^\/v1\/fuses\/runs\/[^/]+$/.test(c.path)).map((c) => c.path));
      expect(reads.size).toBeGreaterThan(0);
      expect(reads.size).toBeLessThanOrEqual(4);
    } finally {
      // Let the reads finish, so none is left in flight for a later test to join.
      never.release();
      await settle(10);
    }
  });

  it('forgets what it learned about a run that left the view, and asks again if it comes back', async () => {
    const server = serve();
    await show();
    await settle(3100);
    await settle(10);
    expect(row('tkt-4162').dataset.guard).toBe('held');
    expect(verdictReads(server, 'tkt-4162')).toHaveLength(1);
    await act(async () => { server.streams[0].push('event: removed\ndata: {"reason":"evicted","run":"tkt-4162","tenant":""}\n\n'); });
    await settle(3000);
    expect(row('tkt-4162')).toBeUndefined();
    // Back, unchanged: what was known about it went when it left.
    const back = listing.runs.find((r) => r.run === 'tkt-4162');
    await act(async () => { server.streams[0].push(`event: run\ndata: ${JSON.stringify(back)}\n\n`); });
    await settle(10);
    expect(row('tkt-4162').dataset.guard).toBe('waiting');
    await settle(3100);
    await settle(10);
    expect(verdictReads(server, 'tkt-4162')).toHaveLength(2);
    expect(row('tkt-4162').dataset.guard).toBe('held');
  });
});
