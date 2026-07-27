import { describe, it, expect } from 'vitest';
import { llmUsage, sessionIdOf, llmMessages, messageText } from './spans.js';

const span = (attributes, extra = {}) => ({ attributes, ...extra });

describe('llmUsage', () => {
  it('prefers current OTel names over the deprecated ones', () => {
    const usage = llmUsage(span({
      'gen_ai.provider.name': 'openai',
      'gen_ai.system': 'legacy',
      'gen_ai.response.model': 'resp',
      'gen_ai.request.model': 'req',
      'gen_ai.usage.input_tokens': 7,
      'gen_ai.usage.prompt_tokens': 999,
      'gen_ai.usage.output_tokens': 3,
      'gen_ai.usage.completion_tokens': 999,
    }));
    expect(usage.provider).toBe('openai');
    expect(usage.model).toBe('resp');
    expect(usage.promptTokens).toBe(7);
    expect(usage.completionTokens).toBe(3);
    expect(usage.totalTokens).toBe(10);
  });

  it('still reads the deprecated names when they are all that is present', () => {
    const usage = llmUsage(span({
      'gen_ai.system': 'anthropic',
      'gen_ai.usage.prompt_tokens': 40,
      'gen_ai.usage.completion_tokens': 25,
    }));
    expect(usage.provider).toBe('anthropic');
    expect(usage.totalTokens).toBe(65);
  });

  it('recognizes an LLM span from operation name alone', () => {
    expect(llmUsage(span({ 'gen_ai.operation.name': 'chat' }))).not.toBeNull();
    expect(llmUsage(span({ 'http.method': 'GET' }))).toBeNull();
  });
});

describe('sessionIdOf', () => {
  it('follows the documented key precedence', () => {
    expect(sessionIdOf(span({ 'session.id': 'a', 'gen_ai.conversation.id': 'b' })))
      .toEqual({ id: 'a', key: 'session.id' });
    expect(sessionIdOf(span({ 'gen_ai.conversation.id': 'b' })))
      .toEqual({ id: 'b', key: 'gen_ai.conversation.id' });
    expect(sessionIdOf(span({ 'traceloop.association.properties.chat_id': 'c' })))
      .toEqual({ id: 'c', key: 'traceloop.association.properties.chat_id' });
  });

  it('stringifies a numeric id, matching the server-side normalizer', () => {
    expect(sessionIdOf(span({ 'gen_ai.conversation.id': 4711 })))
      .toEqual({ id: '4711', key: 'gen_ai.conversation.id' });
  });

  it('returns null when no recognized key is present', () => {
    expect(sessionIdOf(span({ 'user.id': 'u' }))).toBeNull();
  });
});

describe('llmMessages', () => {
  it('parses the current JSON messages with role and parts', () => {
    const messages = llmMessages(span({
      'gen_ai.input.messages': JSON.stringify([
        { role: 'user', parts: [{ type: 'text', content: 'Hi' }] },
      ]),
      'gen_ai.output.messages': JSON.stringify([
        { role: 'assistant', parts: [{ type: 'text', content: 'Hello' }], finish_reason: 'stop' },
      ]),
    }));
    expect(messages).toHaveLength(2);
    expect(messages[0]).toMatchObject({ direction: 'prompt', role: 'user' });
    expect(messages[0].parts[0]).toEqual({ kind: 'text', text: 'Hi' });
    expect(messages[1].finishReason).toBe('stop');
  });

  it('classifies media, and only renders sources a browser can fetch', () => {
    const [message] = llmMessages(span({
      'gen_ai.output.messages': JSON.stringify([{
        role: 'assistant',
        parts: [
          { type: 'image', mime_type: 'image/png', filename: 'a.png', data: 'data:image/png;base64,AAA' },
          { type: 'video', mime_type: 'video/mp4', filename: 'v.mp4', uri: 's3://bucket/v.mp4' },
        ],
      }]),
    }));
    expect(message.parts[0]).toMatchObject({ kind: 'media', mediaType: 'image', src: 'data:image/png;base64,AAA' });
    // An object-store locator is a reference, not a broken image.
    expect(message.parts[1]).toMatchObject({ kind: 'media', mediaType: 'video', src: null, uri: 's3://bucket/v.mp4' });
  });

  it('parses tool calls and results', () => {
    const [message] = llmMessages(span({
      'gen_ai.output.messages': JSON.stringify([{
        role: 'assistant',
        parts: [{ type: 'tool_call', id: 'c1', name: 'lookup', arguments: '{"a":1}' }],
      }]),
    }));
    expect(message.parts[0]).toMatchObject({ kind: 'tool_call', name: 'lookup', id: 'c1' });
  });

  it('falls back to the legacy indexed attributes', () => {
    const messages = llmMessages(span({
      'gen_ai.prompt.0.role': 'user',
      'gen_ai.prompt.0.content': 'legacy in',
      'gen_ai.completion.0.role': 'assistant',
      'gen_ai.completion.0.content': 'legacy out',
    }));
    expect(messages.map((m) => m.parts[0].text)).toEqual(['legacy in', 'legacy out']);
  });

  it('falls back to native llm.prompt / llm.completion events', () => {
    const messages = llmMessages(span({}, {
      events: [
        { name: 'llm.prompt', attributes: { content: 'evt in' } },
        { name: 'llm.completion', attributes: { content: 'evt out' } },
      ],
    }));
    expect(messages.map((m) => m.parts[0].text)).toEqual(['evt in', 'evt out']);
  });

  it('surfaces an offloaded payload instead of rendering nothing', () => {
    const [message] = llmMessages(span({
      'gen_ai.input.messages': { $payload: 'sha256/abc', bytes: 900000, preview: 'start…' },
    }));
    expect(message.parts[0]).toMatchObject({ kind: 'payload', ref: 'sha256/abc', preview: 'start…' });
  });

  it('keeps unparseable content as text rather than dropping it', () => {
    const [message] = llmMessages(span({ 'gen_ai.input.messages': 'not json at all' }));
    expect(message.parts[0]).toEqual({ kind: 'text', text: 'not json at all' });
  });
});

describe('messageText', () => {
  it('renders every part kind for copy-to-clipboard', () => {
    const [message] = llmMessages(span({
      'gen_ai.output.messages': JSON.stringify([{
        role: 'assistant',
        parts: [
          { type: 'text', content: 'see this' },
          { type: 'image', mime_type: 'image/png', filename: 'a.png', uri: 's3://b/a.png' },
        ],
      }]),
    }));
    expect(messageText(message)).toBe('see this\n[image a.png s3://b/a.png]');
  });
});

// ---------------------------------------------------------------- self time
// Added with the rebuilt waterfall: a bar shows where time WAS, self time
// shows where it WENT, and the two differ whenever a span has children.

import { selfTimeNs, criticalPath, childrenOf } from './spans.js';

const node = (id, parent, start, end) => ({
  span_id: id, parent_span_id: parent, trace_id: 't',
  start_time_ns: start, end_time_ns: end, name: id, service: 's', status: 'ok',
});

describe('selfTimeNs', () => {
  it('is the whole duration when a span has no children', () => {
    expect(selfTimeNs(node('a', null, 0, 100), [])).toBe(100);
  });

  it('subtracts a child that covers part of the span', () => {
    expect(selfTimeNs(node('a', null, 0, 100), [node('b', 'a', 10, 60)])).toBe(50);
  });

  it('unions overlapping children rather than summing them', () => {
    // Two concurrent tool calls are the normal case in an agent trace.
    // Summing them would double-count the overlap and report negative
    // self time on a span that was simply waiting on both at once.
    const children = [node('b', 'a', 0, 60), node('c', 'a', 30, 80)];
    expect(selfTimeNs(node('a', null, 0, 100), children)).toBe(20);
  });

  it('never goes negative when children outlast their parent', () => {
    expect(selfTimeNs(node('a', null, 0, 50), [node('b', 'a', 0, 90)])).toBe(0);
  });

  it('counts a gap between children as the parent working', () => {
    const children = [node('b', 'a', 0, 20), node('c', 'a', 60, 80)];
    expect(selfTimeNs(node('a', null, 0, 100), children)).toBe(60);
  });
});

describe('criticalPath', () => {
  it('follows the child that finishes last at each level', () => {
    const spans = [
      node('root', null, 0, 100),
      node('slow', 'root', 10, 90),
      node('fast', 'root', 10, 20),
      node('deep', 'slow', 20, 85),
    ];
    const path = criticalPath(spans);
    expect(path.has('root')).toBe(true);
    expect(path.has('slow')).toBe(true);
    expect(path.has('deep')).toBe(true);
    expect(path.has('fast')).toBe(false);
  });

  it('is empty for an empty trace rather than throwing', () => {
    expect(criticalPath([]).size).toBe(0);
  });
});

describe('childrenOf', () => {
  it('groups spans under their parent and ignores roots', () => {
    const spans = [node('root', null, 0, 10), node('a', 'root', 1, 2), node('b', 'root', 3, 4)];
    const kids = childrenOf(spans);
    expect(kids.get('root').map((s) => s.span_id)).toEqual(['a', 'b']);
    expect(kids.has(null)).toBe(false);
  });
});
